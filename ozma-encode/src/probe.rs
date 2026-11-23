// SPDX-License-Identifier: AGPL-3.0-only
//! Encoder probe results.
//!
//! A capability is only a claim. A probe is evidence: it records which
//! encoder was attempted, whether a real frame encoded successfully, the
//! failure if not, and when the check ran. `EncoderRegistry` uses these
//! results to skip a broken hardware encoder and fall back visibly.

use std::collections::HashMap;

use async_trait::async_trait;
use ozma_media::{Encoder, Frame, FrameFormat};
use ozma_protocol::{codes, ApiError, Codec, EncodeCapability, EncoderBackend, MediaFormat};

use crate::software::{annex_b_is_keyframe, annex_b_nal_types, H264SoftwareEncoder};

/// Outcome of probing one encoder capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// The capability that was probed.
    pub encoder: EncodeCapability,
    /// True only when a frame was actually encoded and validated.
    pub ok: bool,
    /// Structured failure when `ok` is false.
    pub error: Option<ApiError>,
    /// Unix timestamp in seconds, supplied by the prober.
    pub checked_at: u64,
}

impl ProbeResult {
    /// A successful probe.
    pub fn passed(encoder: EncodeCapability, checked_at: u64) -> Self {
        Self {
            encoder,
            ok: true,
            error: None,
            checked_at,
        }
    }

    /// A failed probe with a structured error.
    pub fn failed(encoder: EncodeCapability, error: ApiError, checked_at: u64) -> Self {
        Self {
            encoder,
            ok: false,
            error: Some(error),
            checked_at,
        }
    }

    /// Whether the encoder encoded a real frame.
    pub fn is_ok(&self) -> bool {
        self.ok
    }

    /// Codec of the probed capability.
    pub fn codec(&self) -> Codec {
        self.encoder.codec
    }

    /// Backend of the probed capability.
    pub fn backend(&self) -> EncoderBackend {
        self.encoder.backend
    }
}

/// Something that can probe encoder capabilities.
///
/// [`SoftwareProbe`] feeds a synthetic frame through OpenH264 and verifies a
/// decodable IDR. [`crate::vaapi::VaapiProbe`] initialises the VAAPI device and
/// really encodes a frame with `h264_vaapi`. Both return a [`ProbeResult`];
/// tests use [`FakeProbe`].
#[async_trait]
pub trait EncoderProbe: Send + Sync {
    /// Probe one capability.
    async fn probe(&self, capability: &EncodeCapability) -> ProbeResult;
}

/// A deterministic probe for tests.
#[derive(Clone, Debug)]
pub struct FakeProbe {
    default_ok: bool,
    checked_at: u64,
    results: HashMap<(Codec, EncoderBackend), bool>,
}

impl Default for FakeProbe {
    fn default() -> Self {
        Self::passing()
    }
}

impl FakeProbe {
    /// A probe that passes everything it has not been told to fail.
    pub fn passing() -> Self {
        Self {
            default_ok: true,
            checked_at: 0,
            results: HashMap::new(),
        }
    }

    /// A probe that fails everything it has not been told to pass.
    pub fn failing() -> Self {
        Self {
            default_ok: false,
            checked_at: 0,
            results: HashMap::new(),
        }
    }

    /// Set the timestamp recorded in every result.
    pub fn with_checked_at(mut self, checked_at: u64) -> Self {
        self.checked_at = checked_at;
        self
    }

    /// Make a specific backend/codec fail.
    pub fn fail_for(mut self, codec: Codec, backend: EncoderBackend) -> Self {
        self.results.insert((codec, backend), false);
        self
    }

    /// Make a specific backend/codec pass.
    pub fn pass_for(mut self, codec: Codec, backend: EncoderBackend) -> Self {
        self.results.insert((codec, backend), true);
        self
    }
}

#[async_trait]
impl EncoderProbe for FakeProbe {
    async fn probe(&self, capability: &EncodeCapability) -> ProbeResult {
        let ok = self
            .results
            .get(&(capability.codec, capability.backend))
            .copied()
            .unwrap_or(self.default_ok);
        if ok {
            ProbeResult::passed(capability.clone(), self.checked_at)
        } else {
            ProbeResult::failed(
                capability.clone(),
                ApiError::new(
                    codes::ENCODER_PROBE_FAILED,
                    format!(
                        "Encoder {} ({}) did not produce a valid keyframe.",
                        capability.codec.as_str(),
                        backend_name(capability.backend)
                    ),
                )
                .with_details(capability.codec.as_str().to_owned())
                .with_remediation("Retry the probe, or let the node fall back to another encoder."),
                self.checked_at,
            )
        }
    }
}

fn backend_name(backend: EncoderBackend) -> &'static str {
    match backend {
        EncoderBackend::Vaapi => "vaapi",
        EncoderBackend::Nvenc => "nvenc",
        EncoderBackend::Qsv => "qsv",
        EncoderBackend::V4l2M2m => "v4l2m2m",
        EncoderBackend::Software => "software",
        EncoderBackend::Fake => "fake",
    }
}

/// A probe that actually encodes a synthetic frame with OpenH264.
///
/// It only validates the software H.264 backend: for any other codec or
/// backend it returns a structured `encoder.probe_failed` result instead of
/// pretending to have tested hardware. Tests can keep using [`FakeProbe`];
/// production selection uses this type so "software is available" is evidence,
/// not a claim.
#[derive(Clone, Debug, Default)]
pub struct SoftwareProbe {
    checked_at: u64,
}

impl SoftwareProbe {
    /// A probe that checks the real bundled OpenH264 encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the timestamp recorded in every result.
    pub fn with_checked_at(mut self, checked_at: u64) -> Self {
        self.checked_at = checked_at;
        self
    }
}

#[async_trait]
impl EncoderProbe for SoftwareProbe {
    async fn probe(&self, capability: &EncodeCapability) -> ProbeResult {
        if capability.codec != Codec::H264 || capability.backend != EncoderBackend::Software {
            return ProbeResult::failed(
                capability.clone(),
                probe_error(
                    capability,
                    format!(
                        "The software probe only validates the OpenH264 H.264 backend, not {} ({}).",
                        capability.codec.as_str(),
                        backend_name(capability.backend)
                    ),
                    "Use a backend-specific probe for this capability, or let selection skip it.",
                ),
                self.checked_at,
            );
        }
        if capability.hdr {
            return ProbeResult::failed(
                capability.clone(),
                probe_error(
                    capability,
                    "The software H.264 encoder cannot encode HDR.".to_owned(),
                    "Probe an SDR capability or use a hardware encoder for HDR.",
                ),
                self.checked_at,
            );
        }

        match self.encode_probe_frame(capability).await {
            Ok(()) => ProbeResult::passed(capability.clone(), self.checked_at),
            Err(error) => ProbeResult::failed(capability.clone(), error, self.checked_at),
        }
    }
}

impl SoftwareProbe {
    async fn encode_probe_frame(&self, capability: &EncodeCapability) -> Result<(), ApiError> {
        let max_width = if capability.max_width == 0 {
            64
        } else {
            capability.max_width
        };
        let max_height = if capability.max_height == 0 {
            64
        } else {
            capability.max_height
        };
        let width = max_width.clamp(2, 64) & !1;
        let height = max_height.clamp(2, 64) & !1;
        if width < 2 || height < 2 {
            return Err(probe_error(
                capability,
                format!(
                    "The capability's {}x{} limit is too small to encode.",
                    max_width, max_height
                ),
                "Advertise at least 2x2 pixels, or do not advertise this encoder.",
            ));
        }

        let fps = if capability.max_fps == 0 {
            30
        } else {
            capability.max_fps.min(30)
        };
        let input = FrameFormat::rgb24(width, height);
        let request = MediaFormat::new(Codec::H264, width, height, fps, 500);
        let mut encoder = H264SoftwareEncoder::new(input, request).map_err(|error| {
            probe_error(
                capability,
                format!(
                    "OpenH264 could not initialise for a probe frame: {}",
                    error.message
                ),
                "Check that the bundled OpenH264 library loads on this target.",
            )
        })?;

        let frame =
            Frame::with_data(input, 0, 0, true, probe_pattern(width, height)).map_err(|error| {
                probe_error(
                    capability,
                    format!("Could not build the probe frame: {error}"),
                    "Report this as a bug in the software probe.",
                )
            })?;
        let encoded = encoder.encode(frame).await.map_err(|error| {
            probe_error(
                capability,
                format!("OpenH264 failed to encode the probe frame: {error}"),
                "Check the OpenH264 build and the advertised limits.",
            )
        })?;

        let types = annex_b_nal_types(encoded.data());
        if types.is_empty() {
            return Err(probe_error(
                capability,
                "OpenH264 produced output without an Annex-B start code.".to_owned(),
                "Check the OpenH264 build; the software encoder must emit Annex-B.",
            ));
        }
        if !annex_b_is_keyframe(encoded.data()) || !types.contains(&7) {
            return Err(probe_error(
                capability,
                format!("The probe frame was not a decodable IDR with SPS (NAL types {types:?})."),
                "Check the OpenH264 configuration; the first frame must be an IDR with SPS/PPS.",
            ));
        }
        Ok(())
    }
}

fn probe_error(
    capability: &EncodeCapability,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ApiError {
    ApiError::new(codes::ENCODER_PROBE_FAILED, message)
        .with_details(format!(
            "{} ({})",
            capability.codec.as_str(),
            backend_name(capability.backend)
        ))
        .with_remediation(remediation)
        .retryable()
}

/// A small, non-uniform pattern so the encoder cannot skip the frame as static.
fn probe_pattern(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.push(((x * 4) & 0xFF) as u8);
            pixels.push(((y * 4) & 0xFF) as u8);
            pixels.push((((x + y) * 2) & 0xFF) as u8);
        }
    }
    pixels
}
