// SPDX-License-Identifier: AGPL-3.0-only
//! Consent policy for the machine the node is running on.
//!
//! Consent is a local decision. A headless server or a kiosk has nobody to ask;
//! a workstation may require an explicit local confirmation. The wire protocol
//! carries the session's [`ConsentRequirement`]; this policy decides what that
//! requirement means on this machine.

use serde::{Deserialize, Serialize};

use ozma_protocol::ConsentRequirement;

/// The deployment class of the machine running the node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineClass {
    /// A desktop or laptop with a local operator present.
    #[default]
    Workstation,
    /// A headless or rack machine. Consent cannot be obtained locally.
    Server,
    /// An unattended appliance or sign. Consent cannot be obtained locally.
    Kiosk,
}

impl MachineClass {
    /// Parse the mDNS/registration spelling (`workstation`, `server`, `kiosk`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "workstation" => Some(Self::Workstation),
            "server" => Some(Self::Server),
            "kiosk" => Some(Self::Kiosk),
            _ => None,
        }
    }

    /// The stable lower-case name used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workstation => "workstation",
            Self::Server => "server",
            Self::Kiosk => "kiosk",
        }
    }

    /// Whether a local operator could be asked for consent at all.
    pub fn can_prompt(self) -> bool {
        matches!(self, Self::Workstation)
    }
}

/// Resolves a protocol [`ConsentRequirement`] into a local yes/no decision.
///
/// * [`ConsentRequirement::Never`] never asks.
/// * [`ConsentRequirement::Workstation`] asks only on a workstation that is
///   configured to require it.
/// * [`ConsentRequirement::Always`] always asks, even on a server or kiosk
///   (where the request must be answered out-of-band, for example by the
///   controller or local console).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentPolicy {
    pub machine_class: MachineClass,
    /// When true, sessions whose privacy policy is `Workstation` wait for
    /// consent on this workstation.
    pub workstation_consent: bool,
}

impl ConsentPolicy {
    pub fn new(machine_class: MachineClass, workstation_consent: bool) -> Self {
        Self {
            machine_class,
            workstation_consent,
        }
    }

    /// A policy that never asks on this machine unless the session says
    /// `Always`.
    pub fn for_machine(machine_class: MachineClass) -> Self {
        Self::new(machine_class, false)
    }

    /// Convenience builder used by configuration loading.
    pub fn requiring_workstation_consent(mut self, required: bool) -> Self {
        self.workstation_consent = required;
        self
    }

    /// Whether this machine must obtain consent for the given requirement.
    pub fn requires_consent(&self, requirement: ConsentRequirement) -> bool {
        match requirement {
            ConsentRequirement::Never => false,
            ConsentRequirement::Workstation => {
                matches!(self.machine_class, MachineClass::Workstation) && self.workstation_consent
            }
            ConsentRequirement::Always => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_and_kiosk_never_prompt_for_workstation_sessions() {
        for machine in [MachineClass::Server, MachineClass::Kiosk] {
            let policy = ConsentPolicy::new(machine, true);
            assert!(!policy.requires_consent(ConsentRequirement::Never));
            assert!(!policy.requires_consent(ConsentRequirement::Workstation));
            assert!(policy.requires_consent(ConsentRequirement::Always));
        }
    }

    #[test]
    fn workstation_consent_is_configured() {
        let off = ConsentPolicy::new(MachineClass::Workstation, false);
        assert!(!off.requires_consent(ConsentRequirement::Workstation));

        let on = ConsentPolicy::new(MachineClass::Workstation, true);
        assert!(on.requires_consent(ConsentRequirement::Workstation));
    }

    #[test]
    fn always_wins_on_every_machine_class() {
        for machine in [
            MachineClass::Workstation,
            MachineClass::Server,
            MachineClass::Kiosk,
        ] {
            assert!(ConsentPolicy::for_machine(machine)
                .requires_consent(ConsentRequirement::Always));
        }
    }

    #[test]
    fn machine_class_roundtrips_its_wire_name() {
        for machine in [
            MachineClass::Workstation,
            MachineClass::Server,
            MachineClass::Kiosk,
        ] {
            assert_eq!(MachineClass::parse(machine.as_str()), Some(machine));
            let json = serde_json::to_string(&machine).unwrap();
            assert_eq!(json, format!("\"{}\"", machine.as_str()));
        }
        assert_eq!(MachineClass::parse("laptop"), None);
    }
}
