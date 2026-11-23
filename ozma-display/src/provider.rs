// SPDX-License-Identifier: AGPL-3.0-only
//! The virtual-display provider contract.
//!
//! A provider turns a [`VirtualDisplaySpec`] into an OS display connector. It
//! does not own capture, encoding, or transport: the connector's scanout is a
//! normal capture source elsewhere in the node.
//!
//! The trait is deliberately object-safe so the node can hold an
//! `Arc<dyn VirtualDisplayProvider>` chosen at startup by platform detection.

use std::fmt;

use ozma_protocol::display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayTopology, DpmsState, VirtualDisplayCaps,
    VirtualDisplaySpec,
};
use ozma_protocol::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A broadcast receiver of provider events.
///
/// Providers keep the matching [`broadcast::Sender`] alive for their own
/// lifetime. A subscriber that falls behind sees
/// [`broadcast::error::RecvError::Lagged`] and should re-read
/// [`VirtualDisplayProvider::topology`] instead of assuming it missed nothing.
pub type DisplayEventStream = broadcast::Receiver<DisplayEvent>;

/// Runtime state of one virtual display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    /// Connected and scanning out.
    #[default]
    Online,
    /// DPMS standby.
    Standby,
    /// DPMS suspend.
    Suspend,
    /// DPMS off, or a blanked connector that still exists.
    Off,
    /// The connector has been removed from the topology.
    Removed,
}

impl DisplayState {
    /// Mirror a protocol DPMS state.
    pub fn from_dpms(state: DpmsState) -> Self {
        match state {
            DpmsState::On => DisplayState::Online,
            DpmsState::Standby => DisplayState::Standby,
            DpmsState::Suspend => DisplayState::Suspend,
            DpmsState::Off => DisplayState::Off,
        }
    }

    /// The DPMS equivalent, or `None` once the display is removed.
    pub fn as_dpms(self) -> Option<DpmsState> {
        match self {
            DisplayState::Online => Some(DpmsState::On),
            DisplayState::Standby => Some(DpmsState::Standby),
            DisplayState::Suspend => Some(DpmsState::Suspend),
            DisplayState::Off => Some(DpmsState::Off),
            DisplayState::Removed => None,
        }
    }

    /// True when the display can show a frame.
    pub fn is_active(self) -> bool {
        self == DisplayState::Online
    }
}

impl fmt::Display for DisplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            DisplayState::Online => "online",
            DisplayState::Standby => "standby",
            DisplayState::Suspend => "suspend",
            DisplayState::Off => "off",
            DisplayState::Removed => "removed",
        };
        f.write_str(name)
    }
}

/// Local-side privacy action for a virtual display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyAction {
    /// Leave the connector as it is.
    #[default]
    Unchanged,
    /// Blank the connector while it stays in the topology.
    Blank,
    /// Put the connector into DPMS off.
    DpmsOff,
    /// Remove the connector from the topology.
    Disable,
}

/// Per-display privacy policy.
///
/// This is display-scoped, not session-scoped: the same virtual display can be
/// used by a later session and the local indicator policy must survive it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayPrivacy {
    /// Show a local "remote display active" indicator where the platform can.
    pub indicator: bool,
    /// What to do to the local side of the connector.
    pub local_action: PrivacyAction,
    /// Remove or blank the display when the owning session disconnects.
    pub blank_on_disconnect: bool,
}

impl Default for DisplayPrivacy {
    fn default() -> Self {
        Self {
            indicator: true,
            local_action: PrivacyAction::Unchanged,
            blank_on_disconnect: false,
        }
    }
}

/// A live virtual display as the provider currently sees it.
///
/// This is a snapshot. Re-read [`VirtualDisplayProvider::topology`] or listen
/// for events after a mode change; holding a stale handle is not a lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualDisplay {
    pub id: DisplayId,
    pub spec: VirtualDisplaySpec,
    pub mode: DisplayMode,
    pub state: DisplayState,
}

impl VirtualDisplay {
    pub fn new(
        spec: VirtualDisplaySpec,
        mode: DisplayMode,
        state: DisplayState,
    ) -> Self {
        Self {
            id: spec.id.clone(),
            spec,
            mode,
            state,
        }
    }

    pub fn id(&self) -> &DisplayId {
        &self.id
    }

    pub fn spec(&self) -> &VirtualDisplaySpec {
        &self.spec
    }

    /// The mode the OS currently has selected.
    pub fn mode(&self) -> &DisplayMode {
        &self.mode
    }

    pub fn state(&self) -> DisplayState {
        self.state
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }
}

impl fmt::Display for VirtualDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {}, {})",
            self.id,
            self.mode.label(),
            self.state,
            self.spec.scale_percent
        )
    }
}

/// Platform mechanism that creates OS display connectors.
pub trait VirtualDisplayProvider: Send + Sync {
    /// What this provider can actually do on this machine. Report honestly:
    /// `supported == false` means the UI must offer mirroring instead.
    fn capabilities(&self) -> VirtualDisplayCaps;

    /// Create a connector and publish its EDID.
    fn create(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplay>;

    /// Remove a connector. Removing an unknown display is not an error only if
    /// the provider documents idempotence; the fake provider is idempotent.
    fn destroy(&self, id: DisplayId) -> Result<()>;

    /// Ask the OS to select one of the display's advertised modes.
    fn set_mode(&self, id: DisplayId, mode: DisplayMode) -> Result<()>;

    /// Change DPMS state. Providers that cannot control DPMS return
    /// `display.unsupported`.
    fn set_dpms(&self, id: DisplayId, state: DpmsState) -> Result<()>;

    /// Apply a display-scoped privacy policy.
    fn set_privacy(&self, id: DisplayId, policy: &DisplayPrivacy) -> Result<()>;

    /// Replace the desired layout. The OS remains authoritative; this is the
    /// node's mirror for input mapping and UI preview.
    fn set_topology(&self, topology: DisplayTopology) -> Result<()>;

    /// Read the current layout.
    fn topology(&self) -> Result<DisplayTopology>;

    /// Subscribe to provider events. Each call returns an independent receiver.
    fn events(&self) -> DisplayEventStream;
}
