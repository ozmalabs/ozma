// SPDX-License-Identifier: AGPL-3.0-only
//! Seat supervision contract.
//!
//! This foundation deliberately ships no real process supervisor. Starting an
//! `ozma-node` child (port readiness, log capture, restart policy, signal
//! handling, and teardown) is a later integration step. The trait lets that
//! launcher be added without changing the planner or controller status shape.
//!
//! [`crate::fake::FakeSeatSupervisor`] is the test double. It records state and
//! can inject start failures; it never spawns anything.

use crate::error::MultiseatError;
use crate::seat::{SeatPlan, SeatRuntimeStatus};

/// Starts, stops, and reports seats.
///
/// Implementations must be idempotent where noted and must never report
/// [`SeatRuntimeStatus`] for a seat they did not start unless the state is
/// explicitly a failure.
pub trait SeatSupervisor {
    /// Start one planned seat. Returns an error if the seat is already running
    /// or the launch fails.
    fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError>;

    /// Stop one seat. Returns an error if the seat id is unknown.
    fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError>;

    /// Current status for every known seat.
    fn status(&self) -> Vec<SeatRuntimeStatus>;
}
