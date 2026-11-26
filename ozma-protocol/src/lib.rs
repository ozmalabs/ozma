// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-protocol` — the shared contract between the Ozma node, controller,
//! and native clients.
//!
//! This crate deliberately contains no I/O and no platform code. It defines
//! the capability, session, display, transport, and error vocabulary that all
//! implementations exchange. Wire adapters (HTTP, QUIC, RTSP, Hestia,
//! Moonlight) live in their own crates.

#![forbid(unsafe_code)]

pub mod capability;
pub mod control;
pub mod diagnostics;
pub mod display;
pub mod display_method;
pub mod error;
pub mod ids;
pub mod input;
pub mod media;
pub mod permission;
pub mod scene;
pub mod scene_execution;
pub mod session;
pub mod transport;
pub mod version;
pub mod xpra;
pub mod xpra_output;

pub use capability::{
    AudioCapability, CaptureCapability, CaptureKind, EncodeCapability, EncoderBackend,
    InputCapability, NodeCapabilities, PowerCapability, ResourceBudget, UsbCapability,
};
pub use control::ControlMessage;
pub use diagnostics::{CheckStatus, DiagnosticCheck, DiagnosticReport, RuntimeMetrics};
pub use display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayPlacement, DisplayTopology, DpmsState,
    HdrCapability, HdrMode, Rotation, VirtualDisplayBackend, VirtualDisplayCaps,
    VirtualDisplaySpec,
};
pub use display_method::{
    ActionDef, ContentRequirement, DataField, DataSchema, DisplayMethod, FieldDef, Interaction,
    InteractionTarget, MethodLocation, MethodRegistry, RegisteredMethod, RenderingTier,
};
pub use error::{codes, ApiError, ErrorResponse};
pub use ids::{ClientId, NodeId, SessionId, TrackId};
pub use input::{InputEvent, KeyModifiers, PointerButton, TouchPhase};
pub use media::{AudioFormat, Codec, ColorFormat, MediaFormat};
pub use permission::{Permission, PermissionSet};
pub use session::{
    AudioSpec, ConsentRequest, ConsentRequirement, EndReason, InputSpec, LocalDisplayAction,
    Participant, ParticipantRole, PointerMode, PrivacyPolicy, QualityPreference, Session,
    SessionEvent, SessionSpec, SessionState, TrackKind, TrackSpec, TransportPreference,
};
pub use transport::{
    ChannelDescriptor, ChannelKind, ChannelPriority, SessionOffer, TransportDescriptor,
    TransportMode, WireFormat,
};
pub use version::{ProtocolVersion, CURRENT_PROTOCOL_VERSION};

/// The stable protocol name used in ALPN, mDNS TXT, and API metadata.
pub const PROTOCOL_NAME: &str = "ozma";
/// Compatibility version for payloads that still use the JSON binding.
///
/// Protobuf is canonical for the core protocol; see `proto/ozma/v1` and
/// `ozma-protocol-proto`.
pub const SCHEMA_VERSION: u16 = 1;

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, ApiError>;
