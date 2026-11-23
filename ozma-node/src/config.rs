// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Node configuration derived from the CLI. The node deliberately keeps
//! configuration small: capability detection and profiles do the rest.

use ozma_auth::MachineClass;
use ozma_protocol::NodeId;

/// Effective node configuration.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub name: String,
    pub node_id: NodeId,
    pub hid_udp_port: u16,
    pub http_port: u16,
    pub capabilities: Vec<String>,
    pub hw: String,
    pub fw: String,
    pub register_url: Option<String>,
    pub register_host: Option<String>,
    pub heartbeat_interval_secs: u64,
    pub role: String,
    pub machine_class: MachineClass,
    pub workstation_consent: bool,
}

impl NodeConfig {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|value| value.trim() == capability)
    }

    pub fn node_id_from_name(name: &str) -> NodeId {
        NodeId::new(format!("{name}._ozma._udp.local."))
    }

    /// The local consent policy for this machine.
    pub fn consent_policy(&self) -> ozma_auth::ConsentPolicy {
        ozma_auth::ConsentPolicy::new(self.machine_class, self.workstation_consent)
    }
}
