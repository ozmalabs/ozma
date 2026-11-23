// SPDX-License-Identifier: AGPL-3.0-only
//! Native Ozma client library.
//!
//! Two independent surfaces are exposed:
//!
//! - [`ControllerClient`] speaks the controller's real node API
//!   (`/api/v1/nodes`, `/api/v1/nodes/{id}`, `/api/v1/nodes/{id}/activate`)
//!   over HTTP(S) with rustls and an optional bearer token.
//! - [`NativeSessionClient`] connects directly to a node over the native QUIC
//!   transport, sends `Hello`, consumes `HelloAck`, receives opaque video
//!   datagrams and session events, and sends input or `ReleaseAll`.
//!
//! [`NodeApiClient`] is the node control plane: it creates sessions
//! (`POST /api/v1/sessions`), approves consent
//! (`POST /api/v1/sessions/{id}/approve`), starts them
//! (`POST /api/v1/sessions/{id}/start`), and fetches
//! `GET /api/v1/transport` so a headless client can obtain the self-signed
//! certificate's DER (base64 in JSON) and use it to pin the QUIC connection. A
//! local `--cert` file remains a supported override.
//!
//! The library never decodes video and never requires a controller or capture
//! hardware for its tests.

#![forbid(unsafe_code)]

pub mod cert;
mod controller;
pub mod error;
pub mod frame_output;
mod http;
mod node_api;
pub mod rdp;
pub mod scene_render;
mod session;
pub mod video;
pub mod vnc;
pub mod xpra_peer;

pub use cert::{
    certificate_der_from_base64, certificate_fingerprint, load_certificate, normalize_fingerprint,
    parse_certificate, verify_certificate_fingerprint,
};
pub use controller::{
    ActivateResponse, ControllerClient, ControllerClientBuilder, ControllerNode, NodeList,
};
pub use error::codes;
pub use node_api::{
    fetch_transport_info, FetchedTransportInfo, NodeApiClient, NodeApiClientBuilder,
};
pub use session::{ClientEvent, MediaPacket, NativeClientConfig, NativeSessionClient};

pub use video::{VideoAssembler, VideoAssemblyError, VideoFrame};
