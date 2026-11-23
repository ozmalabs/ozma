// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-display` — virtual-display providers, EDID generation, and input
//! mapping for the Ozma node.
//!
//! The crate is split into four independently testable pieces:
//!
//! * [`provider`] defines the platform-neutral [`VirtualDisplayProvider`]
//!   contract and the display/privacy snapshot types.
//! * [`fake`] is a deterministic in-memory provider for CI and the API layer.
//! * [`mapping`] converts display-local pointer coordinates into host topology
//!   coordinates (and back for edge crossing), including scale and rotation.
//! * [`edid`] builds a minimal, valid EDID 1.4 base block from a display spec.
//!
//! [`evdi`] is a deliberately safe detection scaffold: it reports whether the
//! Linux EVDI prerequisites exist and explains what is missing. It never loads
//! the native library, because that would require `unsafe` FFI. Real provider
//! work is gated on the M2 spike in `docs/remote-display-evdi-spike.md`.
//!
//! The crate performs no privileged operations and contains no `unsafe` code.

#![forbid(unsafe_code)]

pub mod edid;
pub mod error;
pub mod evdi;
pub mod fake;
pub mod mapping;
pub mod provider;

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

/// Re-export of the protocol display vocabulary this crate operates on, so
/// callers do not need to depend on `ozma-protocol` directly for common types.
pub use ozma_protocol::display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, DpmsState,
    HdrCapability, HdrMode, Rotation, VirtualDisplayBackend, VirtualDisplayCaps,
    VirtualDisplaySpec,
};
