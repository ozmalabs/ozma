// SPDX-License-Identifier: AGPL-3.0-only
//! The VM console and multi-display model.
//!
//! A virtual machine does not expose OS display connectors the way a hardware
//! node does. Instead QEMU (or libvirt on its behalf) exposes one display head
//! per console: `Console_0`, `Console_1`, ... over QMP, the QEMU D-Bus display
//! interface, VNC, or Looking Glass. This module models those heads so the node
//! can lay them out for capture selection, input mapping, and UI preview.
//!
//! **A [`VmConsole`] is not a real OS connector.** It is the VM-side display
//! vocabulary. The platform virtual-display provider path (EVDI today) stays in
//! [`crate::evdi`] and is unaffected by this model.
//!
//! The vocabulary mirrors the Python soft node:
//!
//! * `Console_N` — the QEMU D-Bus object name and our [`DisplayId`] suffix.
//! * `Width`/`Height`/`Label` — the console properties read from D-Bus.
//! * `source_type` — one of `qmp`, `dbus`, `vnc`, or `looking_glass`.
//!
//! Layout is deliberately simple and non-authoritative: consoles are placed
//! left-to-right at `x` offsets, `y = 0`, 100% scale, and 0° rotation. The
//! guest OS owns its own framebuffer layout; this is a host-side mirror for
//! mapping a client's display-local input onto the right console.

use std::fmt;
use std::str::FromStr;

use ozma_protocol::display::{DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, Rotation};
use serde::{Deserialize, Serialize};

/// Scale used for every VM console placement. A VM console has no host-side
/// scale factor; capture and input work in the console's own pixels.
pub const DEFAULT_VM_SCALE_PERCENT: u16 = 100;

/// Refresh rate attached to the [`DisplayMode`] synthesized from a console.
///
/// QEMU's console interface exposes width, height, and a label, but not a
/// refresh rate. 60 Hz is a neutral placeholder so callers can use the
/// protocol's mode type; capture pacing is a separate concern.
pub const DEFAULT_VM_REFRESH_HZ: u32 = 60;

/// Upper sanity bound for a console index.
///
/// QEMU's `virtio-gpu` exposes a small number of heads (typically two to
/// four). This ceiling only rejects obviously malformed input; it is not a
/// hardware limit.
pub const MAX_VM_CONSOLES: u32 = 64;

/// How a VM console is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmConsoleBackend {
    /// QMP `screendump` / `input-send-event`.
    Qmp,
    /// QEMU's D-Bus display interface (`org.qemu.Display1`).
    Dbus,
    /// A VNC server attached to the VM.
    Vnc,
    /// Looking Glass shared memory (KVMFR).
    LookingGlass,
}

impl VmConsoleBackend {
    /// Every backend, in declaration order.
    pub const ALL: [Self; 4] = [Self::Qmp, Self::Dbus, Self::Vnc, Self::LookingGlass];

    /// Stable wire/telemetry name. This is the serde representation and must
    /// not change without a protocol version bump.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qmp => "qmp",
            Self::Dbus => "dbus",
            Self::Vnc => "vnc",
            Self::LookingGlass => "looking_glass",
        }
    }
}

impl fmt::Display for VmConsoleBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VmConsoleBackend {
    type Err = VmDisplayError;

    /// Parse a backend name case-insensitively, accepting `-` or `_` as the
    /// word separator (`looking-glass`, `looking_glass`, `LookingGlass`).
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "qmp" => Ok(Self::Qmp),
            "dbus" => Ok(Self::Dbus),
            "vnc" => Ok(Self::Vnc),
            "looking_glass" | "lookingglass" => Ok(Self::LookingGlass),
            _ => Err(VmDisplayError::UnknownBackend(value.trim().to_owned())),
        }
    }
}

/// One VM display head.
///
/// Fields are public to match the protocol structs, but they are not
/// self-validating: call [`VmConsole::validate`] before using a console that
/// was assembled by hand or decoded from a peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConsole {
    /// Zero-based console index, matching QEMU's `Console_N`.
    pub index: u32,
    /// Human-readable console label, normally QEMU's `Label` property.
    pub label: String,
    /// Console width in physical pixels.
    pub width: u32,
    /// Console height in physical pixels.
    pub height: u32,
    /// Transport that reaches this console.
    pub backend: VmConsoleBackend,
    /// Whether the guest designated this console as primary.
    #[serde(default)]
    pub primary: bool,
}

impl VmConsole {
    /// Build a console from explicit parts.
    pub fn new(
        index: u32,
        label: impl Into<String>,
        width: u32,
        height: u32,
        backend: VmConsoleBackend,
        primary: bool,
    ) -> Self {
        Self {
            index,
            label: label.into(),
            width,
            height,
            backend,
            primary,
        }
    }

    /// Build a QMP console with the conventional `Console_N` label.
    pub fn qmp(index: u32, width: u32, height: u32) -> Self {
        Self::new(
            index,
            format!("Console_{index}"),
            width,
            height,
            VmConsoleBackend::Qmp,
            false,
        )
    }

    /// Mark (or clear) this console as primary.
    #[must_use]
    pub fn with_primary(mut self, primary: bool) -> Self {
        self.primary = primary;
        self
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// `(width, height)` in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn backend(&self) -> VmConsoleBackend {
        self.backend
    }

    pub fn is_primary(&self) -> bool {
        self.primary
    }

    /// Stable display identity, matching QEMU's console object name.
    pub fn display_id(&self) -> DisplayId {
        DisplayId::new(format!("Console_{}", self.index))
    }

    /// The console's mode at [`DEFAULT_VM_REFRESH_HZ`].
    pub fn mode(&self) -> DisplayMode {
        DisplayMode::new(self.width, self.height, DEFAULT_VM_REFRESH_HZ)
    }

    /// Check the fields this model depends on. This never panics.
    pub fn validate(&self) -> Result<(), VmDisplayError> {
        if self.index >= MAX_VM_CONSOLES {
            return Err(VmDisplayError::IndexOutOfRange {
                index: self.index,
                max: MAX_VM_CONSOLES,
            });
        }
        if self.label.trim().is_empty() {
            return Err(VmDisplayError::EmptyLabel { index: self.index });
        }
        if self.width == 0 || self.height == 0 {
            return Err(VmDisplayError::ZeroDimension {
                index: self.index,
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }
}

impl fmt::Display for VmConsole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Console_{} ({}, {}x{}, {}{})",
            self.index,
            self.label,
            self.width,
            self.height,
            self.backend,
            if self.primary { ", primary" } else { "" }
        )
    }
}

/// A complete set of VM display heads.
///
/// Ordering is the layout order: [`VmDisplayTopology::placements`] walks
/// `consoles` left-to-right. Construct with [`VmDisplayTopology::new`] (empty
/// builder) or [`VmDisplayTopology::from_consoles`], then call
/// [`VmDisplayTopology::validate`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmDisplayTopology {
    /// Consoles in host layout order.
    pub consoles: Vec<VmConsole>,
}

impl VmDisplayTopology {
    /// An empty topology. Valid as a builder state only: [`Self::validate`]
    /// rejects it until at least one console is added.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a topology from consoles, preserving iteration order.
    pub fn from_consoles(consoles: impl IntoIterator<Item = VmConsole>) -> Self {
        Self {
            consoles: consoles.into_iter().collect(),
        }
    }

    /// Append a console, preserving order.
    #[must_use]
    pub fn with_console(mut self, console: VmConsole) -> Self {
        self.consoles.push(console);
        self
    }

    /// Consoles in layout order.
    pub fn consoles(&self) -> &[VmConsole] {
        &self.consoles
    }

    /// Find a console by its `Console_N` index, not by vector position.
    pub fn get(&self, index: u32) -> Option<&VmConsole> {
        self.consoles.iter().find(|console| console.index == index)
    }

    /// Position in [`Self::consoles`] of the console that acts as primary.
    fn primary_position(&self) -> Option<usize> {
        self.consoles
            .iter()
            .position(|console| console.primary)
            .or_else(|| self.consoles.iter().position(|console| console.index == 0))
            .or_else(|| (!self.consoles.is_empty()).then_some(0))
    }

    /// The explicit primary console, else `Console_0`, else the first console.
    pub fn primary(&self) -> Option<&VmConsole> {
        self.primary_position()
            .map(|position| &self.consoles[position])
    }

    /// Check the whole topology. This never panics.
    pub fn validate(&self) -> Result<(), VmDisplayError> {
        if self.consoles.is_empty() {
            return Err(VmDisplayError::NoConsoles);
        }
        for console in &self.consoles {
            console.validate()?;
        }
        for (position, console) in self.consoles.iter().enumerate() {
            if self.consoles[..position]
                .iter()
                .any(|previous| previous.index == console.index)
            {
                return Err(VmDisplayError::DuplicateIndex(console.index));
            }
        }
        let primaries: Vec<u32> = self
            .consoles
            .iter()
            .filter(|console| console.primary)
            .map(|console| console.index)
            .collect();
        if primaries.len() > 1 {
            return Err(VmDisplayError::MultiplePrimaries { indices: primaries });
        }
        Ok(())
    }

    /// Left-to-right placements: `x` accumulates console widths, `y = 0`,
    /// [`DEFAULT_VM_SCALE_PERCENT`] scale, and 0° rotation.
    ///
    /// Exactly one placement is marked primary, chosen by [`Self::primary`].
    /// Invalid input is normalized rather than rejected here; call
    /// [`Self::validate`] first when the distinction matters.
    pub fn placements(&self) -> Vec<DisplayPlacement> {
        let primary_position = self.primary_position();
        let mut x: i64 = 0;
        let mut placements = Vec::with_capacity(self.consoles.len());
        for (position, console) in self.consoles.iter().enumerate() {
            placements.push(DisplayPlacement {
                display_id: console.display_id(),
                x: i32::try_from(x).unwrap_or(i32::MAX),
                y: 0,
                width: console.width,
                height: console.height,
                scale_percent: DEFAULT_VM_SCALE_PERCENT,
                rotation: Rotation::Deg0,
                primary: primary_position == Some(position),
            });
            x = x.saturating_add(i64::from(console.width));
        }
        placements
    }

    /// The protocol topology mirror for the current consoles.
    ///
    /// This does not validate; an empty topology yields
    /// [`DisplayTopology::default`]. Call [`Self::validate`] first for a
    /// layout that is meant to be authoritative.
    pub fn to_topology(&self) -> DisplayTopology {
        DisplayTopology::new(self.placements())
    }

    /// Mode of a console selected by index (`Console_N`), not vector position.
    pub fn mode_for(&self, index: u32) -> Option<DisplayMode> {
        self.get(index).map(VmConsole::mode)
    }

    /// Modes for every console, in layout order.
    pub fn modes(&self) -> Vec<DisplayMode> {
        self.consoles.iter().map(VmConsole::mode).collect()
    }
}

/// A VM console model failure.
///
/// Messages are stable enough for logs and API details; match on the variant
/// for programmatic handling.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VmDisplayError {
    /// A topology must contain at least one console.
    #[error("a VM display topology needs at least one console")]
    NoConsoles,
    /// Two consoles claimed the same `Console_N` index.
    #[error("duplicate VM console index {0}")]
    DuplicateIndex(u32),
    /// The console index exceeded [`MAX_VM_CONSOLES`].
    #[error("VM console index {index} is out of range (maximum {max})")]
    IndexOutOfRange { index: u32, max: u32 },
    /// A console label was empty or whitespace only.
    #[error("VM console {index} needs a non-empty label")]
    EmptyLabel { index: u32 },
    /// A console had a zero width or height.
    #[error("VM console {index} has an invalid size {width}x{height}")]
    ZeroDimension { index: u32, width: u32, height: u32 },
    /// More than one console was explicitly marked primary.
    #[error("multiple VM consoles are marked primary: indices {indices:?}")]
    MultiplePrimaries { indices: Vec<u32> },
    /// A backend name was not recognized.
    #[error("unknown VM console backend {0:?}")]
    UnknownBackend(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(index: u32, width: u32, height: u32) -> VmConsole {
        VmConsole::qmp(index, width, height)
    }

    #[test]
    fn single_console_layout() {
        let topology = VmDisplayTopology::from_consoles([console(0, 1920, 1080)]);
        assert!(topology.validate().is_ok());

        let placements = topology.placements();
        assert_eq!(placements.len(), 1);
        let placement = &placements[0];
        assert_eq!(placement.display_id, DisplayId::new("Console_0"));
        assert_eq!((placement.x, placement.y), (0, 0));
        assert_eq!((placement.width, placement.height), (1920, 1080));
        assert_eq!(placement.scale_percent, 100);
        assert_eq!(placement.rotation, Rotation::Deg0);
        assert!(placement.primary);

        assert_eq!(topology.to_topology(), DisplayTopology::new(placements));
        assert_eq!(topology.primary().unwrap().index(), 0);
        assert_eq!(
            topology.mode_for(0),
            Some(DisplayMode::new(1920, 1080, DEFAULT_VM_REFRESH_HZ))
        );
    }

    #[test]
    fn three_consoles_are_laid_out_left_to_right() {
        let topology = VmDisplayTopology::from_consoles([
            console(0, 1920, 1080),
            console(1, 1280, 1024),
            console(2, 1024, 768),
        ]);
        assert!(topology.validate().is_ok());

        let placements = topology.placements();
        let xs: Vec<i32> = placements.iter().map(|placement| placement.x).collect();
        assert_eq!(xs, vec![0, 1920, 3200]);
        assert!(placements.iter().all(|placement| placement.y == 0));
        assert_eq!(
            placements
                .iter()
                .map(|placement| (placement.width, placement.height))
                .collect::<Vec<_>>(),
            vec![(1920, 1080), (1280, 1024), (1024, 768)]
        );

        assert_eq!(topology.modes().len(), 3);
        assert_eq!(topology.modes()[1].width, 1280);
    }

    #[test]
    fn layout_follows_vector_order_not_index_order() {
        let topology =
            VmDisplayTopology::from_consoles([console(1, 800, 600), console(0, 1024, 768)]);
        assert!(topology.validate().is_ok());

        let placements = topology.placements();
        assert_eq!(placements[0].x, 0);
        assert_eq!(placements[0].display_id, DisplayId::new("Console_1"));
        assert_eq!(placements[1].x, 800);
        assert_eq!(placements[1].display_id, DisplayId::new("Console_0"));

        // Implicit primary is Console_0 even though it is second in the layout.
        assert!(!placements[0].primary);
        assert!(placements[1].primary);
        assert_eq!(topology.primary().unwrap().index(), 0);
    }

    #[test]
    fn explicit_primary_wins_over_index_zero() {
        let topology = VmDisplayTopology::from_consoles([
            console(0, 1920, 1080),
            console(1, 1280, 1024).with_primary(true),
        ]);
        assert!(topology.validate().is_ok());

        assert_eq!(topology.primary().unwrap().index(), 1);
        let placements = topology.placements();
        assert!(!placements[0].primary);
        assert!(placements[1].primary);
        assert_eq!(
            topology.to_topology().primary().unwrap().display_id,
            DisplayId::new("Console_1")
        );
    }

    #[test]
    fn duplicate_index_is_rejected() {
        let topology =
            VmDisplayTopology::from_consoles([console(1, 1920, 1080), console(1, 1280, 720)]);
        assert_eq!(topology.validate(), Err(VmDisplayError::DuplicateIndex(1)));
    }

    #[test]
    fn zero_dimension_is_rejected() {
        let zero_width = VmConsole::new(0, "Console 0", 0, 1080, VmConsoleBackend::Qmp, false);
        assert_eq!(
            zero_width.validate(),
            Err(VmDisplayError::ZeroDimension {
                index: 0,
                width: 0,
                height: 1080,
            })
        );

        let zero_height = VmConsole::new(0, "Console 0", 1920, 0, VmConsoleBackend::Qmp, false);
        assert_eq!(
            zero_height.validate(),
            Err(VmDisplayError::ZeroDimension {
                index: 0,
                width: 1920,
                height: 0,
            })
        );

        let topology = VmDisplayTopology::from_consoles([zero_width]);
        assert_eq!(
            topology.validate(),
            Err(VmDisplayError::ZeroDimension {
                index: 0,
                width: 0,
                height: 1080,
            })
        );
    }

    #[test]
    fn empty_label_is_rejected() {
        let console = VmConsole::new(0, "   ", 1920, 1080, VmConsoleBackend::Qmp, false);
        assert_eq!(
            console.validate(),
            Err(VmDisplayError::EmptyLabel { index: 0 })
        );
    }

    #[test]
    fn index_out_of_range_is_rejected() {
        let console = console(MAX_VM_CONSOLES, 1920, 1080);
        assert_eq!(
            console.validate(),
            Err(VmDisplayError::IndexOutOfRange {
                index: MAX_VM_CONSOLES,
                max: MAX_VM_CONSOLES,
            })
        );
    }

    #[test]
    fn multiple_primaries_are_rejected() {
        let topology = VmDisplayTopology::from_consoles([
            console(0, 1920, 1080).with_primary(true),
            console(1, 1280, 1024).with_primary(true),
        ]);
        assert_eq!(
            topology.validate(),
            Err(VmDisplayError::MultiplePrimaries {
                indices: vec![0, 1],
            })
        );
    }

    #[test]
    fn empty_topology_is_rejected_but_safe_to_place() {
        let topology = VmDisplayTopology::new();
        assert_eq!(topology.validate(), Err(VmDisplayError::NoConsoles));
        assert!(topology.primary().is_none());
        assert!(topology.placements().is_empty());
        assert_eq!(topology.to_topology(), DisplayTopology::default());
    }

    #[test]
    fn placements_use_default_scale_and_rotation() {
        let topology =
            VmDisplayTopology::from_consoles([console(0, 2560, 1440), console(1, 1920, 1080)]);
        for placement in topology.placements() {
            assert_eq!(placement.scale_percent, DEFAULT_VM_SCALE_PERCENT);
            assert_eq!(placement.rotation, Rotation::Deg0);
            assert_eq!(placement.y, 0);
        }
    }

    #[test]
    fn topology_serde_round_trip() {
        let topology = VmDisplayTopology::from_consoles([
            VmConsole::new(0, "QEMU VGA", 1920, 1080, VmConsoleBackend::Dbus, true),
            console(1, 1280, 1024),
            VmConsole::new(
                2,
                "Looking Glass",
                2560,
                1440,
                VmConsoleBackend::LookingGlass,
                false,
            ),
        ]);
        assert!(topology.validate().is_ok());

        let json = serde_json::to_string(&topology).unwrap();
        let decoded: VmDisplayTopology = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, topology);
    }

    #[test]
    fn backend_names_round_trip() {
        for backend in VmConsoleBackend::ALL {
            assert_eq!(
                backend.as_str().parse::<VmConsoleBackend>().unwrap(),
                backend
            );
            assert_eq!(backend.to_string(), backend.as_str());

            let json = serde_json::to_string(&backend).unwrap();
            assert_eq!(json, format!("\"{}\"", backend.as_str()));
            assert_eq!(
                serde_json::from_str::<VmConsoleBackend>(&json).unwrap(),
                backend
            );
        }

        assert_eq!(
            "looking-glass".parse::<VmConsoleBackend>().unwrap(),
            VmConsoleBackend::LookingGlass
        );
        assert_eq!(
            " DBUS ".parse::<VmConsoleBackend>().unwrap(),
            VmConsoleBackend::Dbus
        );
        assert_eq!(
            "telepathy".parse::<VmConsoleBackend>(),
            Err(VmDisplayError::UnknownBackend("telepathy".to_owned()))
        );
    }

    #[test]
    fn console_display_ids_follow_console_n_vocabulary() {
        let topology =
            VmDisplayTopology::from_consoles([console(0, 800, 600), console(7, 1024, 768)]);
        assert_eq!(topology.get(0).unwrap().display_id().as_str(), "Console_0");
        assert_eq!(topology.get(7).unwrap().display_id().as_str(), "Console_7");
        assert!(topology.get(2).is_none());
    }
}
