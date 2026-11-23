// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-encode` — encoder selection, probing, and a real software H.264
//! backend.
//!
//! This crate now contains the M1 software media path:
//!
//! * [`H264SoftwareEncoder`] implements [`ozma_media::Encoder`] using the
//!   bundled OpenH264 library. It converts MJPEG, NV12, YUYV, RGB24, and BGRA
//!   capture frames to I420, emits Annex-B H.264 access units, preserves PTS
//!   and sequence numbers, supports forced keyframes, and can pass an existing
//!   H.264 Annex-B stream through.
//! * [`SoftwareProbe`] encodes a synthetic frame and verifies that OpenH264
//!   produced a real IDR access unit with a start code and SPS. [`FakeProbe`]
//!   remains for deterministic tests.
//! * [`EncoderRegistry`] selects a backend deterministically, with hardware
//!   preference and a software fallback. [`EncoderSelection::build_encoder`]
//!   builds the real encoder for a software selection.
//!
//! # What is still not implemented
//!
//! VAAPI, NVENC, QSV, and V4L2M2M are not implemented here. A selection that
//! names one of those backends returns a structured `encoder.unavailable`
//! error from [`EncoderSelection::build_encoder`] rather than pretending to
//! encode. [`ozma_media::FakeEncoder`] and [`FakeProbe`] stay available for CI
//! and onboarding tests, but are never substituted for a hardware claim.
//!
//! `ffmpeg` detection remains a non-fatal capability hint; no FFmpeg sidecar is
//! used for encoding. The software codec is OpenH264, and the crate itself
//! stays `#![forbid(unsafe_code)]` (the OpenH264 binding is the only place with
//! native code).

#![forbid(unsafe_code)]

pub mod ffmpeg;
pub mod probe;
pub mod registry;
pub mod software;

pub use ffmpeg::{detect_ffmpeg, FfmpegStatus};
pub use probe::{EncoderProbe, FakeProbe, ProbeResult, SoftwareProbe};
pub use registry::{
    backend_name, EncoderRegistry, EncoderSelection, SelectedEncoder, SelectionPolicy,
    SelectionReason,
};
pub use software::{
    annex_b_is_keyframe, annex_b_nal_types, H264SoftwareEncoder, ENCODE_FAILED, MAX_FPS,
    MAX_HEIGHT, MAX_WIDTH,
};

/// Re-export of protocol types callers usually need alongside selection.
pub use ozma_protocol::{Codec, EncodeCapability, EncoderBackend, MediaFormat};
