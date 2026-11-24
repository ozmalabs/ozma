// SPDX-License-Identifier: AGPL-3.0-only
//! Host topology enumeration for the CLI and tests.
//!
//! [`HostTopology`] combines one display backend and one input backend. It is
//! deliberately strict: a backend error is propagated, and an empty display
//! list is [`MultiseatError::NoDisplays`] rather than a plan with fabricated
//! seats. An empty input list is not an error; the planner records
//! `NoInputDevices` and plans seats without keyboard/mouse ownership.
//!
//! Tests construct this with [`crate::fake::FakeDisplayBackend`] and
//! [`crate::fake::FakeInputBackend`]; production code passes
//! [`crate::display::XrandrDisplayBackend`] and
//! [`crate::input::SysfsInputBackend`].

use crate::display::{DisplayBackend, DisplayOutput};
use crate::error::MultiseatError;
use crate::input::{InputBackend, InputDevice};
use crate::planner::MultiseatPlanner;
use crate::seat::MultiseatPlan;

/// The displays and input devices one host enumeration produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTopology {
    /// Usable display outputs.
    pub displays: Vec<DisplayOutput>,
    /// Classified input devices (possibly empty).
    pub inputs: Vec<InputDevice>,
    /// Display backend name.
    pub display_backend: String,
    /// Input backend name.
    pub input_backend: String,
}

impl HostTopology {
    /// Enumerate both backends.
    ///
    /// Display enumeration failure, an empty display list, or input
    /// enumeration failure is returned to the caller. The function never
    /// substitutes test fakes or invents a display.
    pub fn enumerate(
        display_backend: &dyn DisplayBackend,
        input_backend: &dyn InputBackend,
    ) -> Result<Self, MultiseatError> {
        let displays = display_backend.enumerate()?;
        if displays.is_empty() {
            return Err(MultiseatError::NoDisplays);
        }
        let inputs = input_backend.enumerate()?;

        Ok(Self {
            displays,
            inputs,
            display_backend: display_backend.backend_name().to_owned(),
            input_backend: input_backend.backend_name().to_owned(),
        })
    }

    /// Plan seats from this topology.
    ///
    /// The plan carries the backends that actually produced the topology, not
    /// the planner's generic "provided" label.
    pub fn plan(&self, planner: &MultiseatPlanner) -> Result<MultiseatPlan, MultiseatError> {
        let mut plan = planner.plan(&self.displays, &self.inputs)?;
        plan.display_backend = self.display_backend.clone();
        plan.input_backend = self.input_backend.clone();
        Ok(plan)
    }
}
