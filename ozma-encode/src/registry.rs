// SPDX-License-Identifier: AGPL-3.0-only
//! Encoder capability selection with hardware preference and software fallback.
//!
//! Selection is intentionally deterministic and explainable: it filters by
//! codec, dimensions, frame rate, HDR requirement, and recorded probe failures,
//! then chooses the best hardware candidate. Software is used only when no
//! hardware candidate is eligible, and the result says so.

use std::cmp::Reverse;
use std::collections::HashMap;

use async_trait::async_trait;
use ozma_media::{EncodedFrame, Encoder, FakeEncoder, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, Codec, EncodeCapability, EncoderBackend, MediaFormat};

use crate::probe::{EncoderProbe, ProbeResult};
use crate::software::H264SoftwareEncoder;
use crate::vaapi::{unix_now, VaapiH264Encoder, VaapiProbe};

fn probe_key(capability: &EncodeCapability) -> (Codec, EncoderBackend) {
    (capability.codec, capability.backend)
}

/// Stable backend name for logs and fake encoder ids.
pub fn backend_name(backend: EncoderBackend) -> &'static str {
    match backend {
        EncoderBackend::Vaapi => "vaapi",
        EncoderBackend::Nvenc => "nvenc",
        EncoderBackend::Qsv => "qsv",
        EncoderBackend::V4l2M2m => "v4l2m2m",
        EncoderBackend::Software => "software",
        EncoderBackend::Fake => "fake",
    }
}

/// Why a candidate was selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionReason {
    /// A hardware encoder was chosen despite software being available.
    HardwarePreferred,
    /// No hardware encoder was eligible, so software was chosen.
    SoftwareFallback,
    /// Only one class of encoder could satisfy the request.
    OnlyCandidate,
}

/// A chosen encoder plus the evidence and request it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderSelection {
    /// The media format the caller asked for.
    pub requested: MediaFormat,
    /// The capability that will be used.
    pub capability: EncodeCapability,
    /// Why this capability was chosen.
    pub reason: SelectionReason,
    /// Probe evidence, when one has been recorded.
    pub probe: Option<ProbeResult>,
}

impl EncoderSelection {
    /// Whether the selection is hardware-backed.
    pub fn is_hardware(&self) -> bool {
        self.capability.hardware
    }

    /// Selected codec.
    pub fn codec(&self) -> Codec {
        self.capability.codec
    }

    /// Selected backend.
    pub fn backend(&self) -> EncoderBackend {
        self.capability.backend
    }

    /// Build a deterministic [`FakeEncoder`] matching this selection.
    ///
    /// This is the integration point for CI and for the onboarding harness:
    /// the same selection logic drives a fake encode path without claiming a
    /// real codec.
    pub fn fake_encoder(&self, input: FrameFormat) -> FakeEncoder {
        FakeEncoder::new(input, self.requested.clone())
            .with_id(format!(
                "fake-{}-{}",
                self.capability.codec.as_str(),
                backend_name(self.capability.backend)
            ))
            .with_keyframe_interval(u64::from(self.requested.fps.max(1)))
    }

    /// Build the concrete software H.264 encoder this selection refers to.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ApiError`] when the selection is not backed by
    /// [`EncoderBackend::Software`], or when the encoder cannot accept `input`.
    pub fn software_encoder(&self, input: FrameFormat) -> Result<H264SoftwareEncoder, ApiError> {
        if self.capability.backend != EncoderBackend::Software {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "Selection `{}` is backed by {} ({}), not the software H.264 encoder.",
                    self.requested.label(),
                    self.capability.codec.as_str(),
                    backend_name(self.capability.backend)
                ),
            )
            .with_details(backend_name(self.capability.backend).to_owned())
            .with_remediation(
                "Select a software capability, or use a backend adapter for this hardware.",
            ));
        }
        H264SoftwareEncoder::new(input, self.requested.clone())
    }

    /// Build the concrete VAAPI H.264 encoder this selection refers to.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ApiError`] when the selection is not backed by
    /// [`EncoderBackend::Vaapi`], or when the render node or `ffmpeg` cannot
    /// be used.
    pub fn vaapi_encoder(&self, input: FrameFormat) -> Result<VaapiH264Encoder, ApiError> {
        if self.capability.backend != EncoderBackend::Vaapi {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "Selection `{}` is backed by {} ({}), not VAAPI.",
                    self.requested.label(),
                    self.capability.codec.as_str(),
                    backend_name(self.capability.backend)
                ),
            )
            .with_details(backend_name(self.capability.backend).to_owned())
            .with_remediation("Select a VAAPI capability, or use the software encoder."));
        }
        VaapiH264Encoder::new(input, self.requested.clone())
    }

    /// Build the encoder for this selection.
    ///
    /// Software selections produce a real [`H264SoftwareEncoder`]; VAAPI
    /// selections produce a real [`VaapiH264Encoder`]; fake selections produce
    /// [`FakeEncoder`] for CI. NVENC, QSV, and V4L2M2M return a structured error
    /// because this crate does not implement them; it never silently
    /// substitutes a fake backend for a hardware claim.
    pub fn build_encoder(&self, input: FrameFormat) -> Result<SelectedEncoder, ApiError> {
        match self.capability.backend {
            EncoderBackend::Software => {
                Ok(SelectedEncoder::Software(self.software_encoder(input)?))
            }
            EncoderBackend::Vaapi => Ok(SelectedEncoder::Vaapi(self.vaapi_encoder(input)?)),
            EncoderBackend::Fake => Ok(SelectedEncoder::Fake(self.fake_encoder(input))),
            other => Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "The `{}` backend is advertised but not implemented by `ozma-encode`.",
                    backend_name(other)
                ),
            )
            .with_details(backend_name(other).to_owned())
            .with_remediation(
                "Use the software H.264 fallback, or add a backend adapter for this hardware.",
            )),
        }
    }
}

/// The concrete encoder produced by [`EncoderSelection::build_encoder`].
///
/// This is an enum rather than a boxed trait object so it satisfies
/// `ozma_media::Pipeline::start`'s `E: Encoder + 'static` bound directly. A
/// caller that only needs the real backend can use
/// [`EncoderSelection::software_encoder`] instead.
#[derive(Debug)]
pub enum SelectedEncoder {
    /// A real OpenH264 software H.264 encoder.
    Software(H264SoftwareEncoder),
    /// A real VAAPI H.264 encoder.
    Vaapi(VaapiH264Encoder),
    /// The deterministic CI encoder.
    Fake(FakeEncoder),
}

#[async_trait]
impl Encoder for SelectedEncoder {
    fn id(&self) -> &str {
        match self {
            Self::Software(encoder) => encoder.id(),
            Self::Vaapi(encoder) => encoder.id(),
            Self::Fake(encoder) => encoder.id(),
        }
    }

    fn input_format(&self) -> FrameFormat {
        match self {
            Self::Software(encoder) => encoder.input_format(),
            Self::Vaapi(encoder) => encoder.input_format(),
            Self::Fake(encoder) => encoder.input_format(),
        }
    }

    fn output_format(&self) -> MediaFormat {
        match self {
            Self::Software(encoder) => encoder.output_format(),
            Self::Vaapi(encoder) => encoder.output_format(),
            Self::Fake(encoder) => encoder.output_format(),
        }
    }

    async fn encode(&mut self, frame: Frame) -> Result<EncodedFrame, MediaError> {
        match self {
            Self::Software(encoder) => encoder.encode(frame).await,
            Self::Vaapi(encoder) => encoder.encode(frame).await,
            Self::Fake(encoder) => encoder.encode(frame).await,
        }
    }

    async fn request_keyframe(&mut self) -> Result<(), MediaError> {
        match self {
            Self::Software(encoder) => encoder.request_keyframe().await,
            Self::Vaapi(encoder) => encoder.request_keyframe().await,
            Self::Fake(encoder) => encoder.request_keyframe().await,
        }
    }

    async fn flush(&mut self) -> Result<Vec<EncodedFrame>, MediaError> {
        match self {
            Self::Software(encoder) => encoder.flush().await,
            Self::Vaapi(encoder) => encoder.flush().await,
            Self::Fake(encoder) => encoder.flush().await,
        }
    }
}

/// Selection constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPolicy {
    /// Permit a software encoder when no hardware candidate qualifies.
    pub allow_software_fallback: bool,
    /// Require a recorded, successful probe before a candidate is eligible.
    pub require_probe: bool,
}

impl Default for SelectionPolicy {
    fn default() -> Self {
        Self {
            allow_software_fallback: true,
            require_probe: false,
        }
    }
}

impl SelectionPolicy {
    /// Default policy: hardware first, software allowed, probes optional.
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuse software-only results.
    pub fn hardware_only(mut self) -> Self {
        self.allow_software_fallback = false;
        self
    }

    /// Ignore capabilities that have not passed a probe.
    pub fn require_probed(mut self) -> Self {
        self.require_probe = true;
        self
    }
}

/// A set of advertised encoders plus their latest probe results.
#[derive(Clone, Debug, Default)]
pub struct EncoderRegistry {
    capabilities: Vec<EncodeCapability>,
    probes: HashMap<(Codec, EncoderBackend), ProbeResult>,
}

impl EncoderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry containing only the real software H.264 fallback.
    ///
    /// This is the useful default on a node without hardware encoder adapters:
    /// it advertises exactly what `ozma-encode` can actually run, and
    /// [`EncoderRegistry::select`] therefore returns a software selection whose
    /// [`EncoderSelection::build_encoder`] is a working OpenH264 encoder.
    pub fn with_software_h264() -> Self {
        Self::from_capabilities([H264SoftwareEncoder::capability()])
    }

    /// A registry with the software fallback plus VAAPI H.264 only when a real
    /// VAAPI probe succeeds.
    ///
    /// This is the production default for the M1 media path. On a host without
    /// a render node, without `ffmpeg`, or without a libva encoder entrypoint
    /// it returns exactly [`EncoderRegistry::with_software_h264`] and records
    /// the failed VAAPI probe for diagnostics; it never advertises a hardware
    /// backend that was not proven by encoding a frame. Selection therefore
    /// prefers VAAPI when it is proven and otherwise selects
    /// [`H264SoftwareEncoder`].
    pub async fn with_detected_encoders() -> Self {
        let mut registry = Self::with_software_h264();
        registry.detect_vaapi().await;
        registry
    }

    /// Probe VAAPI and advertise it only if the probe passed.
    ///
    /// Returns the probe result for logging or diagnostics. A failed probe is
    /// recorded but does not register the capability, so it cannot be
    /// selected.
    pub async fn detect_vaapi(&mut self) -> ProbeResult {
        let capability = VaapiH264Encoder::capability();
        let result = VaapiProbe::new()
            .with_checked_at(unix_now())
            .probe(&capability)
            .await;
        let ok = result.is_ok();
        self.register_probe(result.clone());
        if ok {
            self.register(capability);
        }
        result
    }

    /// Build a registry from advertised capabilities.
    pub fn from_capabilities(capabilities: impl IntoIterator<Item = EncodeCapability>) -> Self {
        let mut registry = Self::new();
        for capability in capabilities {
            registry.register(capability);
        }
        registry
    }

    /// Register or replace a capability.
    ///
    /// A `(codec, backend)` pair is unique: re-registering it replaces the old
    /// limit so a hotplug or driver reload cannot leave stale duplicates.
    pub fn register(&mut self, capability: EncodeCapability) {
        if let Some(existing) = self
            .capabilities
            .iter_mut()
            .find(|existing| probe_key(existing) == probe_key(&capability))
        {
            *existing = capability;
        } else {
            self.capabilities.push(capability);
        }
    }

    /// Advertised capabilities in registration order.
    pub fn capabilities(&self) -> &[EncodeCapability] {
        &self.capabilities
    }

    /// True when any capability advertises `codec`.
    pub fn has_codec(&self, codec: Codec) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.codec == codec)
    }

    /// Latest probe result for a codec/backend pair.
    pub fn probe_result(&self, codec: Codec, backend: EncoderBackend) -> Option<&ProbeResult> {
        self.probes.get(&(codec, backend))
    }

    /// Record a probe result. The newest result wins.
    pub fn register_probe(&mut self, result: ProbeResult) {
        self.probes
            .insert((result.encoder.codec, result.encoder.backend), result);
    }

    /// Builder form of [`EncoderRegistry::register_probe`].
    pub fn with_probe(mut self, result: ProbeResult) -> Self {
        self.register_probe(result);
        self
    }

    /// Probe every advertised capability and record the results.
    pub async fn probe_all(&mut self, probe: &dyn EncoderProbe) {
        let capabilities = self.capabilities.clone();
        let mut results = Vec::with_capacity(capabilities.len());
        for capability in &capabilities {
            results.push(probe.probe(capability).await);
        }
        for result in results {
            self.register_probe(result);
        }
    }

    /// Choose the best encoder for `requested` with the default policy.
    pub fn select(&self, requested: &MediaFormat) -> Result<EncoderSelection, ApiError> {
        self.select_with(requested, SelectionPolicy::default())
    }

    /// Choose the best encoder for `requested` under an explicit policy.
    pub fn select_with(
        &self,
        requested: &MediaFormat,
        policy: SelectionPolicy,
    ) -> Result<EncoderSelection, ApiError> {
        if requested.width == 0 || requested.height == 0 {
            return Err(ApiError::invalid(
                "The requested media format must have non-zero dimensions.",
            ));
        }

        let mut codec_matches = 0usize;
        let mut had_hardware_candidate = false;
        let mut eligible: Vec<&EncodeCapability> = Vec::new();

        for capability in &self.capabilities {
            if capability.codec != requested.codec {
                continue;
            }
            codec_matches += 1;
            if capability.hardware {
                had_hardware_candidate = true;
            }
            if capability.max_width < requested.width || capability.max_height < requested.height {
                continue;
            }
            if requested.fps > 0 && capability.max_fps < requested.fps {
                continue;
            }
            if (requested.hdr || requested.color.is_hdr()) && !capability.hdr {
                continue;
            }
            match self.probes.get(&probe_key(capability)) {
                Some(result) if !result.ok => continue,
                None if policy.require_probe => continue,
                _ => {}
            }
            eligible.push(capability);
        }

        if eligible.is_empty() {
            return Err(self.no_candidate_error(requested, codec_matches, had_hardware_candidate));
        }

        let hardware: Vec<&EncodeCapability> = eligible
            .iter()
            .copied()
            .filter(|capability| capability.hardware)
            .collect();
        let software: Vec<&EncodeCapability> = eligible
            .iter()
            .copied()
            .filter(|capability| !capability.hardware)
            .collect();

        let (chosen, reason) = if !hardware.is_empty() {
            let chosen = best(&hardware, &self.probes);
            let reason = if software.is_empty() {
                SelectionReason::OnlyCandidate
            } else {
                SelectionReason::HardwarePreferred
            };
            (chosen, reason)
        } else if !software.is_empty() {
            if !policy.allow_software_fallback {
                return Err(ApiError::new(
                    codes::ENCODER_UNAVAILABLE,
                    "No hardware encoder is available and the software fallback is disabled.",
                )
                .with_details(requested.label())
                .with_remediation(
                    "Enable the software fallback or choose a codec with hardware support.",
                ));
            }
            let chosen = best(&software, &self.probes);
            let reason = if had_hardware_candidate {
                SelectionReason::SoftwareFallback
            } else {
                SelectionReason::OnlyCandidate
            };
            (chosen, reason)
        } else {
            // Unreachable: `eligible` was non-empty and every candidate is
            // either hardware or software.
            return Err(self.no_candidate_error(requested, codec_matches, had_hardware_candidate));
        };

        Ok(EncoderSelection {
            requested: requested.clone(),
            capability: chosen.clone(),
            reason,
            probe: self.probes.get(&probe_key(chosen)).cloned(),
        })
    }

    fn no_candidate_error(
        &self,
        requested: &MediaFormat,
        codec_matches: usize,
        had_hardware_candidate: bool,
    ) -> ApiError {
        if codec_matches == 0 {
            return ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "No encoder on this node supports codec `{}`.",
                    requested.codec.as_str()
                ),
            )
            .with_details(requested.codec.as_str().to_owned())
            .with_remediation(
                "Request a codec this node advertises, or install and configure a backend.",
            );
        }

        if had_hardware_candidate {
            return ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!(
                    "No encoder could satisfy {} after probing; hardware candidates failed or could not meet the format.",
                    requested.label()
                ),
            )
            .with_details(requested.label())
            .with_remediation("Retry the probe, or allow another hardware or software encoder.")
            .retryable();
        }

        ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!("No encoder can satisfy {}.", requested.label()),
        )
        .with_details(requested.label())
        .with_remediation("Lower the resolution or frame rate, or enable another encoder.")
    }
}

/// Pick the best candidate: probed-passing first, then larger output, then
/// higher FPS, then the preferred backend order.
fn best<'a>(
    candidates: &[&'a EncodeCapability],
    probes: &HashMap<(Codec, EncoderBackend), ProbeResult>,
) -> &'a EncodeCapability {
    candidates
        .iter()
        .copied()
        .max_by_key(|capability| {
            let probed = probes
                .get(&probe_key(capability))
                .map(|result| result.ok)
                .unwrap_or(true);
            (
                probed,
                capability.max_width.saturating_mul(capability.max_height),
                capability.max_fps,
                Reverse(capability.backend),
            )
        })
        .expect("selection is only called with a non-empty candidate list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::FakeProbe;
    use ozma_media::CaptureSource;
    use ozma_media::Encoder;
    use ozma_media::FakeCapture;

    fn capability(
        codec: Codec,
        backend: EncoderBackend,
        hardware: bool,
        width: u32,
        height: u32,
        fps: u32,
        hdr: bool,
    ) -> EncodeCapability {
        EncodeCapability {
            codec,
            backend,
            hardware,
            max_width: width,
            max_height: height,
            max_fps: fps,
            hdr,
            profiles: Vec::new(),
        }
    }

    fn h264_request() -> MediaFormat {
        MediaFormat::new(Codec::H264, 1920, 1080, 30, 12_000)
    }

    fn h264_vaapi() -> EncodeCapability {
        capability(
            Codec::H264,
            EncoderBackend::Vaapi,
            true,
            3840,
            2160,
            60,
            false,
        )
    }

    fn h264_software() -> EncodeCapability {
        capability(
            Codec::H264,
            EncoderBackend::Software,
            false,
            1920,
            1080,
            30,
            false,
        )
    }

    #[test]
    fn hardware_is_preferred_over_software() {
        let registry = EncoderRegistry::from_capabilities([h264_software(), h264_vaapi()]);
        let selection = registry.select(&h264_request()).unwrap();
        assert_eq!(selection.backend(), EncoderBackend::Vaapi);
        assert!(selection.is_hardware());
        assert_eq!(selection.reason, SelectionReason::HardwarePreferred);
    }

    #[tokio::test]
    async fn software_is_chosen_after_hardware_probe_failure() {
        let mut registry = EncoderRegistry::from_capabilities([h264_vaapi(), h264_software()]);
        registry
            .probe_all(&FakeProbe::passing().fail_for(Codec::H264, EncoderBackend::Vaapi))
            .await;

        let selection = registry.select(&h264_request()).unwrap();
        assert_eq!(selection.backend(), EncoderBackend::Software);
        assert!(!selection.is_hardware());
        assert_eq!(selection.reason, SelectionReason::SoftwareFallback);
        assert!(selection.probe.as_ref().unwrap().is_ok());
        assert!(!registry
            .probe_result(Codec::H264, EncoderBackend::Vaapi)
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn all_failed_probes_report_a_probe_error() {
        let mut registry = EncoderRegistry::from_capabilities([h264_vaapi(), h264_software()]);
        registry.probe_all(&FakeProbe::failing()).await;
        let error = registry.select(&h264_request()).unwrap_err();
        assert_eq!(error.code, codes::ENCODER_PROBE_FAILED);
        assert!(error.retryable);
    }

    #[test]
    fn unsupported_codec_returns_a_structured_error() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let request = MediaFormat::new(Codec::Av1, 1920, 1080, 30, 8_000);
        let error = registry.select(&request).unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
        assert_eq!(error.details.as_str(), Some("av1"));
    }

    #[test]
    fn resolution_fps_and_hdr_are_enforced() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let too_large = MediaFormat::new(Codec::H264, 3840, 2160, 30, 20_000);
        assert_eq!(
            registry.select(&too_large).unwrap_err().code,
            codes::ENCODER_UNAVAILABLE
        );

        let too_fast = MediaFormat::new(Codec::H264, 1920, 1080, 120, 20_000);
        assert_eq!(
            registry.select(&too_fast).unwrap_err().code,
            codes::ENCODER_UNAVAILABLE
        );

        let mut hdr_request = MediaFormat::new(Codec::H264, 1920, 1080, 30, 20_000);
        hdr_request.hdr = true;
        assert_eq!(
            registry.select(&hdr_request).unwrap_err().code,
            codes::ENCODER_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn hardware_only_policy_refuses_software() {
        let mut registry = EncoderRegistry::from_capabilities([h264_vaapi(), h264_software()]);
        registry
            .probe_all(&FakeProbe::passing().fail_for(Codec::H264, EncoderBackend::Vaapi))
            .await;
        let error = registry
            .select_with(&h264_request(), SelectionPolicy::new().hardware_only())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
    }

    #[tokio::test]
    async fn detected_registry_only_advertises_proven_vaapi() {
        let registry = EncoderRegistry::with_detected_encoders().await;
        let advertised = registry
            .capabilities()
            .iter()
            .any(|capability| capability.backend == EncoderBackend::Vaapi);
        let probe = registry.probe_result(Codec::H264, EncoderBackend::Vaapi);
        assert_eq!(
            advertised,
            probe.map(ProbeResult::is_ok).unwrap_or(false),
            "VAAPI advertisement and probe evidence must agree"
        );
        assert!(
            registry
                .capabilities()
                .iter()
                .any(|capability| capability.backend == EncoderBackend::Software),
            "the software fallback must always be advertised"
        );

        let selection = registry.select(&h264_request()).unwrap();
        if advertised {
            assert_eq!(selection.backend(), EncoderBackend::Vaapi);
            assert!(selection.is_hardware());
        } else {
            assert_eq!(selection.backend(), EncoderBackend::Software);
            assert!(!selection.is_hardware());
        }
    }

    #[test]
    fn vaapi_encoder_builder_rejects_software_selection() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let selection = registry.select(&h264_request()).unwrap();
        let error = selection
            .vaapi_encoder(FrameFormat::rgb24(320, 240))
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
    }

    #[tokio::test]
    async fn selection_builds_a_working_fake_encoder() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let request = MediaFormat::new(Codec::H264, 320, 240, 30, 2_000);
        let selection = registry.select(&request).unwrap();
        assert_eq!(selection.reason, SelectionReason::OnlyCandidate);

        let mut encoder = selection.fake_encoder(FrameFormat::rgb24(320, 240));
        let mut source = FakeCapture::default().with_frame_limit(1).with_fps(1_000);
        let frame = source.next_frame().await.unwrap();
        let encoded = encoder.encode(frame).await.unwrap();
        assert_eq!(encoded.format().codec, Codec::H264);
        assert!(!encoded.is_empty());
        assert!(encoded.is_keyframe());
    }
}
