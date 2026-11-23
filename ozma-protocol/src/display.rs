// SPDX-License-Identifier: AGPL-3.0-only
//! Display topology, virtual-display specifications, and display events.
//!
//! A virtual display is a real OS connector where the platform permits it.
//! The protocol describes the desired connector and its placement; it never
//! pretends that an overlay window is a monitor.

use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::ids::{ClientId, SessionId};

pub use crate::ids::DisplayId;

/// Rotation of a display in the host topology.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum Rotation {
    #[default]
    #[serde(rename = "0")]
    Deg0,
    #[serde(rename = "90")]
    Deg90,
    #[serde(rename = "180")]
    Deg180,
    #[serde(rename = "270")]
    Deg270,
}

impl Rotation {
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 90,
            Rotation::Deg180 => 180,
            Rotation::Deg270 => 270,
        }
    }

    /// Swap width/height when the display is rotated into portrait.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Deg90 | Rotation::Deg270)
    }
}

/// HDR mode advertised by a panel or node.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HdrMode {
    #[default]
    None,
    Hdr10,
    Hdr10Plus,
    DolbyVision,
}

impl HdrMode {
    pub fn is_hdr(self) -> bool {
        !matches!(self, HdrMode::None)
    }
}

/// A panel mode. `refresh_hz` is an integer because EDID and most display
/// APIs describe refresh in whole hertz; fractional modes are represented by
/// the nearest integer and a `reduced_blanking` flag where needed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    #[serde(default)]
    pub hdr: HdrMode,
    #[serde(default)]
    pub reduced_blanking: bool,
}

impl DisplayMode {
    pub fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width,
            height,
            refresh_hz,
            hdr: HdrMode::None,
            reduced_blanking: false,
        }
    }

    pub fn hdr(mut self, hdr: HdrMode) -> Self {
        self.hdr = hdr;
        self
    }

    pub fn reduced_blanking(mut self, enabled: bool) -> Self {
        self.reduced_blanking = enabled;
        self
    }

    pub fn label(&self) -> String {
        format!("{}x{}@{}", self.width, self.height, self.refresh_hz)
    }
}

impl std::fmt::Display for DisplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// HDR details a panel can accept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HdrCapability {
    #[serde(default)]
    pub modes: Vec<HdrMode>,
    #[serde(default)]
    pub max_luminance_nits: Option<u32>,
    #[serde(default)]
    pub min_luminance_millinit: Option<u32>,
}

impl HdrCapability {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn hdr10(max_luminance_nits: u32) -> Self {
        Self {
            modes: vec![HdrMode::Hdr10],
            max_luminance_nits: Some(max_luminance_nits),
            min_luminance_millinit: None,
        }
    }

    pub fn supports(&self, mode: HdrMode) -> bool {
        mode == HdrMode::None || self.modes.contains(&mode)
    }
}

/// What a platform can actually create. `false` means the UI must offer a
/// labelled mirror instead of promising an extra monitor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualDisplayCaps {
    pub supported: bool,
    #[serde(default)]
    pub backends: Vec<VirtualDisplayBackend>,
    #[serde(default)]
    pub max_outputs: u16,
    #[serde(default)]
    pub hotplug: bool,
    /// True when the provider can publish a client-chosen EDID; many HDMI
    /// capture dongles cannot.
    #[serde(default)]
    pub programmable_edid: bool,
    /// True when a disconnect/reconnect keeps the same monitor identity.
    #[serde(default)]
    pub stable_identity: bool,
    #[serde(default)]
    pub hdr: HdrCapability,
    #[serde(default)]
    pub rotation: bool,
    #[serde(default)]
    pub scale: bool,
}

impl VirtualDisplayCaps {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn fake() -> Self {
        Self {
            supported: true,
            backends: vec![VirtualDisplayBackend::Fake],
            max_outputs: 4,
            hotplug: true,
            programmable_edid: true,
            stable_identity: true,
            hdr: HdrCapability::hdr10(400),
            rotation: true,
            scale: true,
        }
    }
}

/// Platform mechanism behind a virtual display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualDisplayBackend {
    Evdi,
    Kms,
    Compositor,
    #[serde(rename = "iddcx")]
    IddCx,
    #[serde(rename = "cg_virtual_display")]
    CgVirtualDisplay,
    #[serde(rename = "hdmi_sink")]
    HdmiSink,
    Fake,
}

/// A request to create a remote display on the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualDisplaySpec {
    pub id: DisplayId,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub client_id: Option<ClientId>,
    pub name: String,
    pub edid_serial: String,
    pub physical_mm: (u32, u32),
    /// Preferred mode first. The provider may publish more than one.
    pub modes: Vec<DisplayMode>,
    pub preferred: DisplayMode,
    #[serde(default)]
    pub hdr: HdrCapability,
    #[serde(default)]
    pub rotation: Rotation,
    /// 100 = 100%. Integer percent avoids float equality and JSON drift.
    #[serde(default = "default_scale")]
    pub scale_percent: u16,
    #[serde(default)]
    pub audio: bool,
}

fn default_scale() -> u16 {
    100
}

impl VirtualDisplaySpec {
    pub fn new(
        id: impl Into<DisplayId>,
        name: impl Into<String>,
        physical_mm: (u32, u32),
        preferred: DisplayMode,
    ) -> Self {
        let id = id.into();
        let name = name.into();
        Self {
            edid_serial: format!("OZMA-{id}"),
            id,
            session_id: None,
            client_id: None,
            name,
            physical_mm,
            modes: vec![preferred.clone()],
            preferred,
            hdr: HdrCapability::none(),
            rotation: Rotation::Deg0,
            scale_percent: 100,
            audio: false,
        }
    }

    pub fn display_name(&self) -> String {
        format!("OZMA-{}", self.id)
    }

    /// Validate the request before a provider touches the OS.
    pub fn validate(&self) -> crate::Result<()> {
        if self.id.is_empty() {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "A virtual display needs a non-empty id",
            ));
        }
        if self.name.trim().is_empty() {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "A virtual display needs a name",
            ));
        }
        if self.modes.is_empty() {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "A virtual display needs at least one mode",
            ));
        }
        if self.preferred.width == 0 || self.preferred.height == 0 || self.preferred.refresh_hz == 0
        {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "A virtual display mode must have non-zero dimensions and refresh",
            ));
        }
        if !self.modes.contains(&self.preferred) {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "The preferred mode must also be listed in modes",
            ));
        }
        if !(25..=300).contains(&self.scale_percent) {
            return Err(ApiError::new(
                crate::error::codes::INVALID_REQUEST,
                "Scale must be between 25% and 300%",
            ));
        }
        Ok(())
    }
}

/// One display's placement in the host topology. The OS remains the source of
/// truth; this is a mirror for input mapping and UI preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayPlacement {
    pub display_id: DisplayId,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_scale")]
    pub scale_percent: u16,
    #[serde(default)]
    pub rotation: Rotation,
    #[serde(default)]
    pub primary: bool,
}

impl DisplayPlacement {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }
}

/// The current desktop topology as reported by the provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayTopology {
    #[serde(default)]
    pub displays: Vec<DisplayPlacement>,
}

impl DisplayTopology {
    pub fn new(displays: Vec<DisplayPlacement>) -> Self {
        Self { displays }
    }

    pub fn find(&self, id: &DisplayId) -> Option<&DisplayPlacement> {
        self.displays.iter().find(|d| &d.display_id == id)
    }

    pub fn primary(&self) -> Option<&DisplayPlacement> {
        self.displays
            .iter()
            .find(|d| d.primary)
            .or_else(|| self.displays.first())
    }

    /// Bounding box of all displays, used for cursor edge calculations.
    pub fn bounds(&self) -> Option<(i32, i32, i32, i32)> {
        let first = self.displays.first()?;
        let mut min_x = first.x;
        let mut min_y = first.y;
        let mut max_x = first.x + first.width as i32;
        let mut max_y = first.y + first.height as i32;
        for display in self.displays.iter().skip(1) {
            min_x = min_x.min(display.x);
            min_y = min_y.min(display.y);
            max_x = max_x.max(display.x + display.width as i32);
            max_y = max_y.max(display.y + display.height as i32);
        }
        Some((min_x, min_y, max_x, max_y))
    }
}

/// Power state of a display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DpmsState {
    #[default]
    On,
    Standby,
    Suspend,
    Off,
}

/// Events a provider emits when the OS changes a virtual display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DisplayEvent {
    Added {
        display: VirtualDisplaySpec,
    },
    Removed {
        display_id: DisplayId,
    },
    ModeChanged {
        display_id: DisplayId,
        mode: DisplayMode,
    },
    TopologyChanged {
        topology: DisplayTopology,
    },
    DpmsChanged {
        display_id: DisplayId,
        state: DpmsState,
    },
    Error {
        display_id: Option<DisplayId>,
        error: ApiError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_display_specs() {
        let mode = DisplayMode::new(1920, 1200, 60);
        let spec = VirtualDisplaySpec::new("laptop-1", "Remote laptop", (300, 190), mode);
        assert!(spec.validate().is_ok());
        assert_eq!(spec.display_name(), "OZMA-laptop-1");
    }

    #[test]
    fn rejects_preferred_mode_not_in_list() {
        let mut spec = VirtualDisplaySpec::new(
            "laptop-1",
            "Remote laptop",
            (300, 190),
            DisplayMode::new(1920, 1200, 60),
        );
        spec.modes.clear();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn topology_finds_and_bounds() {
        let topology = DisplayTopology::new(vec![
            DisplayPlacement {
                display_id: DisplayId::new("a"),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: true,
            },
            DisplayPlacement {
                display_id: DisplayId::new("b"),
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
                scale_percent: 100,
                rotation: Rotation::Deg0,
                primary: false,
            },
        ]);
        assert!(topology.find(&DisplayId::new("b")).is_some());
        assert_eq!(topology.bounds(), Some((0, 0, 3200, 1080)));
    }
}
