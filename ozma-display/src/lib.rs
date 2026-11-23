// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-display` — virtual-display providers, EDID generation, and input
//! mapping for the Ozma node.
//!
//! The crate is split into independently testable pieces:
//!
//! * [`provider`] defines the platform-neutral [`VirtualDisplayProvider`]
//!   contract and the display/privacy snapshot types.
//! * [`fake`] is a deterministic in-memory provider for CI and the API layer.
//! * [`mapping`] converts display-local pointer coordinates into host topology
//!   coordinates (and back for edge crossing), including scale and rotation.
//! * [`edid`] builds a minimal, valid EDID 1.4 base block from a display spec.
//! * [`vm`] models the display consoles a VM exposes over QMP, the QEMU D-Bus
//!   display interface, VNC, or Looking Glass. A VM console is not an OS
//!   connector: the model supplies a deterministic multi-display layout for
//!   capture selection, input mapping, and UI preview.
//!
//! [`evdi`] remains the platform virtual-display provider path: it is a
//! deliberately safe detection scaffold that reports whether the Linux EVDI
//! prerequisites exist and explains what is missing. It never loads the native
//! library, because that would require `unsafe` FFI. Real provider work is
//! gated on the M2 spike in `docs/remote-display-evdi-spike.md`. The VM console
//! model lives beside it and does not replace it.
//!
//! The crate performs no privileged operations and contains no `unsafe` code.

#![forbid(unsafe_code)]

pub mod edid;
pub mod error;
pub mod evdi;
pub mod fake;
pub mod mapping;
pub mod provider;
pub mod vm;

pub use edid::{build as build_edid, Edid, EDID_LEN};
pub use error::DISPLAY_OUT_OF_BOUNDS;
pub use evdi::{EvdiProbe, EvdiProvider};
pub use fake::FakeVirtualDisplayProvider;
pub use mapping::{
    display_at, edge_crossing, global_to_local, local_to_global, logical_size, map_absolute,
    map_absolute_input, map_relative_input, relative_passthrough, unmap_absolute, GlobalPoint,
    LocalPoint, RelativeDelta,
};
pub use provider::{
    DisplayEventStream, DisplayPrivacy, DisplayState, PrivacyAction, VirtualDisplay,
    VirtualDisplayProvider,
};
pub use vm::{
    VmConsole, VmConsoleBackend, VmDisplayError, VmDisplayTopology, DEFAULT_VM_REFRESH_HZ,
    DEFAULT_VM_SCALE_PERCENT, MAX_VM_CONSOLES,
};

/// Re-export of the protocol display vocabulary this crate operates on, so
/// callers do not need to depend on `ozma-protocol` directly for common types.
pub use ozma_protocol::display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, DpmsState,
    HdrCapability, HdrMode, Rotation, VirtualDisplayBackend, VirtualDisplayCaps,
    VirtualDisplaySpec,
};
