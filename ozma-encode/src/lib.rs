// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-encode` — encoder selection, probing, and real software and VAAPI
//! H.264 backends.
//!
//! This crate contains the M1 media path, with a hardware path where the host
//! proves it works:
//!
//! * [`H264SoftwareEncoder`] implements [`ozma_media::Encoder`] using the
//!   bundled OpenH264 library. It converts MJPEG, NV12, YUYV, RGB24, and BGRA
//!   capture frames to I420, emits Annex-B H.264 access units, preserves PTS
//!   and sequence numbers, supports forced keyframes, and can pass an existing
//!   H.264 Annex-B stream through.
//! * [`SoftwareProbe`] encodes a synthetic frame and verifies that OpenH264
//!   produced a real IDR access unit with a start code and SPS. [`FakeProbe`]
//!   remains for deterministic tests.
//! * [`VaapiH264Encoder`] implements the same trait with real VAAPI hardware
//!   encoding through a supervised FFmpeg sidecar (`h264_vaapi`). It converts
//!   the same capture layouts to I420, extracts deterministic Annex-B access
//!   units from explicitly length-prefixed FLV tags, preserves PTS/sequence,
//!   detects IDRs from the NAL units, forces keyframes by restarting the
//!   encoder, and kills the child process on drop. See the module docs for the
//!   exact path and why FFI is not used.
//! * [`VaapiProbe`] initialises the device and encodes a real probe frame. It
//!   never accepts a software encode as hardware evidence.
//! * [`EncoderRegistry`] selects a backend deterministically, with hardware
//!   preference and a software fallback.
//!   [`EncoderRegistry::with_detected_encoders`] advertises VAAPI only after
//!   [`VaapiProbe`] passes, and keeps [`H264SoftwareEncoder`] as the fallback.
//!   [`EncoderSelection::build_encoder`] builds the real encoder for software,
//!   VAAPI, and fake selections.
//!
//! # What is still not implemented
//!
//! NVENC, QSV, and V4L2M2M are not implemented here. A selection that names
//! one of those backends returns a structured `encoder.unavailable` error from
//! [`EncoderSelection::build_encoder`] rather than pretending to encode.
//! [`ozma_media::FakeEncoder`] and [`FakeProbe`] stay available for CI and
//! onboarding tests, but are never substituted for a hardware claim.
//!
//! The crate stays `#![forbid(unsafe_code)]`: the OpenH264 binding and the
//! `ffmpeg` process boundary are the only places native work happens, and both
//! are dependencies or subprocesses rather than unsafe Rust in this crate.

#![forbid(unsafe_code)]

pub mod ffmpeg;
pub mod probe;
pub mod registry;
pub mod software;
pub mod vaapi;

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
pub use vaapi::{
    detect_vaapi_device, ffmpeg_binary, VaapiH264Encoder, VaapiProbe, DEFAULT_VAAPI_DEVICE,
    FFMPEG_ENV, VAAPI_DEVICE_ENV, VAAPI_MAX_FPS, VAAPI_MAX_HEIGHT, VAAPI_MAX_WIDTH,
};

/// Re-export of protocol types callers usually need alongside selection.
pub use ozma_protocol::{Codec, EncodeCapability, EncoderBackend, MediaFormat};
