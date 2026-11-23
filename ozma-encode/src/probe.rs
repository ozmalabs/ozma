// SPDX-License-Identifier: AGPL-3.0-only
//! Encoder probe results.
//!
//! A capability is only a claim. A probe is evidence: it records which
//! encoder was attempted, whether a real frame encoded successfully, the
//! failure if not, and when the check ran. `EncoderRegistry` uses these
//! results to skip a broken hardware encoder and fall back visibly.

use std::collections::HashMap;

use async_trait::async_trait;
use ozma_protocol::{codes, ApiError, Codec, EncodeCapability, EncoderBackend};

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
/// The M1 implementation will feed a real captured frame through the backend,
/// verify a decodable keyframe, and return a [`ProbeResult`]. Tests use
/// [`FakeProbe`].
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
