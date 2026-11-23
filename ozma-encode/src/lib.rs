// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-encode` — encoder capability selection and probing.
//!
//! Real backends (VAAPI, NVENC, QSV, V4L2M2M, software) land in M1. This crate
//! provides the contract they plug into: a registry of advertised
//! [`ozma_protocol::EncodeCapability`] values, a `ProbeResult` that records
//! evidence, deterministic selection with hardware preference and software
//! fallback, and a bridge to `ozma_media::FakeEncoder` for CI.
//!
//! No encoder here claims to compress video. `FakeEncoder` is explicitly fake,
//! and `ffmpeg` detection is only a non-fatal capability hint.

#![forbid(unsafe_code)]

pub mod ffmpeg;
pub mod probe;
pub mod registry;

pub use ffmpeg::{detect_ffmpeg, FfmpegStatus};
pub use probe::{EncoderProbe, FakeProbe, ProbeResult};
pub use registry::{
    backend_name, EncoderRegistry, EncoderSelection, SelectionPolicy, SelectionReason,
};

/// Re-export of protocol types callers usually need alongside selection.
pub use ozma_protocol::{Codec, EncodeCapability, EncoderBackend, MediaFormat};
