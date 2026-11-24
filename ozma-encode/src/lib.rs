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
//!   [`EncoderRegistry::with_detected_encoders`] probes the host and advertises
//!   only encoders whose device evidence is present: software H.264 always,
//!   VAAPI H.264/HEVC after a real device probe, NVENC after an ffmpeg listing
//!   plus a CUDA/NVML node, and QSV/V4L2M2M only after a real one-frame encode.
//!   [`EncoderRegistry::with_detected_encoders_for_device`] honours an explicit
//!   `--vaapi-device` instead of the default render node.
//!   [`EncoderSelection::build_encoder`] builds the real encoder for software,
//!   VAAPI, and fake selections; use
//!   [`EncoderRegistry::select_buildable`] when the caller needs a concrete
//!   `ozma-encode` encoder rather than an HLS-only backend.
//! * [`CodecPreference`] / [`EncoderOverride`] express a runtime codec
//!   preference, and [`EncoderRegistry::select_with_override`] resolves it
//!   against the advertised capabilities. The policy is pure and honest: an
//!   explicit hardware request is honoured only when the registry advertises
//!   that exact backend, a software result is never reported as hardware, and
//!   software fallback happens only when
//!   [`CodecPreference::allow_software_fallback`] is set. Unavailable requests
//!   return a typed error listing the capabilities that really exist.
//!   [`EncoderRegistry::evaluate_override`] and
//!   [`EncoderRegistry::codec_state`] produce [`OverrideReceipt`] /
//!   [`CodecOverrideState`] values that a node can store and expose on
//!   `GET /codec`.
//!
//! # What is still not implemented
//!
//! NVENC, QSV, and V4L2M2M are not implemented here. A selection that names
//! one of those backends returns a structured `encoder.unavailable` error from
//! [`EncoderSelection::build_encoder`] rather than pretending to encode.
//! [`ozma_media::FakeEncoder`] and [`FakeProbe`] stay available for CI and
//! onboarding tests, but are never substituted for a hardware claim.
//!
//! The override policy in this crate only describes and validates a requested
//! selection. It does not restart an encode pipeline, and it does not open an
//! HTTP endpoint: `POST /codec` and the pipeline restart are the node's
//! responsibility, and a node must not report an override as live until the
//! real encoder has restarted with the selected capability.
//!
//! The crate stays `#![forbid(unsafe_code)]`: the OpenH264 binding and the
//! `ffmpeg` process boundary are the only places native work happens, and both
//! are dependencies or subprocesses rather than unsafe Rust in this crate.

#![forbid(unsafe_code)]

pub mod ffmpeg;
pub mod jpeg;
pub mod override_policy;
pub mod probe;
pub mod registry;
pub mod software;
pub mod vaapi;

pub use ffmpeg::{detect_ffmpeg, FfmpegStatus};
pub use jpeg::{encode_jpeg_bgra, encode_jpeg_frame, encode_jpeg_rgb, JpegError};
pub use override_policy::{
    backend_is_hardware, backend_supports_codec, CodecOverrideState, CodecPreference,
    EncoderOverride, FallbackReason, OverrideReceipt, OverrideResult,
};
pub use probe::{EncoderProbe, FakeProbe, ProbeResult, SoftwareProbe};
pub use registry::{
    backend_name, EncoderEnvironment, EncoderRegistry, EncoderSelection, SelectedEncoder,
    SelectionPolicy, SelectionReason,
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
