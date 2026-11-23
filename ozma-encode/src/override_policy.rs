// SPDX-License-Identifier: AGPL-3.0-only
//! Runtime codec override policy.
//!
//! This module turns a controller's codec/backend preference into a concrete
//! selection from an [`EncoderRegistry`]. It is deliberately pure: it does not
//! touch a pipeline, spawn an encoder, or write state. The node owns that I/O;
//! it can store [`CodecOverrideState`] and expose the honest result on
//! `GET /codec` after it has restarted the encode pipeline.
//!
//! The rules are intentionally conservative:
//!
//! * An explicit backend is honoured only when the registry advertises that
//!   exact `(codec, backend)` pair, the capability meets the requested format,
//!   and its recorded probe has not failed.
//! * A hardware request never turns into a hardware claim. If only a software
//!   capability is available, the result carries that real software
//!   capability, sets [`OverrideResult::applied`] to `false`, and records a
//!   [`FallbackReason`].
//! * Software fallback happens only when
//!   [`CodecPreference::allow_software_fallback`] is true.
//! * Without fallback the call fails with a typed [`ApiError`]
//!   (`encoder.unavailable` or `encoder.probe_failed`) whose `details` list
//!   the capabilities that really exist.
//!
//! [`OverrideResult::applied`] describes the selection policy only. A node
//! must not report an override as live until its pipeline has actually
//! restarted with the selected capability.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use ozma_protocol::{codes, ApiError, Codec, EncodeCapability, EncoderBackend, MediaFormat};

use crate::registry::{
    backend_name, EncoderRegistry, EncoderSelection, SelectionPolicy, SelectionReason,
};

/// Whether a backend is a hardware encoder class.
///
/// Software and fake backends are never hardware, even when they encode a
/// frame successfully.
pub fn backend_is_hardware(backend: EncoderBackend) -> bool {
    matches!(
        backend,
        EncoderBackend::Vaapi
            | EncoderBackend::Nvenc
            | EncoderBackend::Qsv
            | EncoderBackend::V4l2M2m
    )
}

/// Whether `backend` can plausibly carry `codec`.
///
/// Video codecs may be paired with any backend the registry advertises. Audio
/// codecs are not hardware-encoded by this crate, so naming a hardware backend
/// for them is a request-shape error rather than a capability miss.
pub fn backend_supports_codec(codec: Codec, backend: EncoderBackend) -> bool {
    codec.is_video() || !backend_is_hardware(backend)
}

/// A controller's requested encode preference.
///
/// The codec is always required. `backend` pins a specific implementation;
/// without it the registry chooses among all backends for that codec using its
/// normal hardware-first policy. `bitrate_kbps`, when present, replaces the
/// bitrate in the caller's [`MediaFormat`]. `allow_software_fallback` is
/// `false` by default so a request for hardware cannot silently degrade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecPreference {
    /// Requested codec.
    pub codec: Codec,
    /// Optional exact backend; `None` lets the registry choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<EncoderBackend>,
    /// Optional bitrate override in kbit/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    /// Permit a software capability when the requested hardware is missing.
    #[serde(default)]
    pub allow_software_fallback: bool,
}

impl CodecPreference {
    /// A codec-only preference with no backend pin and no software fallback.
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            backend: None,
            bitrate_kbps: None,
            allow_software_fallback: false,
        }
    }

    /// Pin a specific backend.
    pub fn with_backend(mut self, backend: EncoderBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Override the requested bitrate.
    pub fn with_bitrate_kbps(mut self, bitrate_kbps: u32) -> Self {
        self.bitrate_kbps = Some(bitrate_kbps);
        self
    }

    /// Permit or forbid software fallback.
    pub fn with_software_fallback(mut self, allow: bool) -> Self {
        self.allow_software_fallback = allow;
        self
    }

    /// Validate the request before it reaches selection.
    ///
    /// # Errors
    ///
    /// Returns `invalid_request` when the bitrate is zero or when a hardware
    /// backend is named for a non-video codec.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.bitrate_kbps == Some(0) {
            return Err(
                ApiError::invalid("A codec override bitrate must be greater than zero.")
                    .with_details(json!({ "bitrate_kbps": 0, "codec": self.codec.as_str() }))
                    .with_remediation(
                        "Omit the bitrate to keep the requested format, or use a positive value.",
                    ),
            );
        }

        if let Some(backend) = self.backend {
            if !backend_supports_codec(self.codec, backend) {
                return Err(ApiError::invalid(format!(
                    "Codec `{}` cannot be encoded by the `{}` backend.",
                    self.codec.as_str(),
                    backend_name(backend)
                ))
                .with_details(json!({
                    "codec": self.codec.as_str(),
                    "backend": backend_name(backend),
                }))
                .with_remediation(
                    "Name a video codec for a hardware backend, or omit the backend preference.",
                ));
            }
        }

        Ok(())
    }

    /// Apply this preference to a caller's format.
    ///
    /// The codec always comes from the preference; the bitrate is replaced only
    /// when the preference names one. Dimensions, frame rate, and HDR state are
    /// left to the caller. This does not validate the preference; call
    /// [`CodecPreference::validate`] first when the input is untrusted.
    pub fn effective_format(&self, requested: &MediaFormat) -> MediaFormat {
        let mut effective = requested.clone();
        effective.codec = self.codec;
        if let Some(bitrate_kbps) = self.bitrate_kbps {
            effective.bitrate_kbps = bitrate_kbps;
        }
        effective
    }
}

/// A requested preference plus its provenance.
///
/// `version` and `epoch` let a node reject stale responses: a later epoch wins
/// even when responses arrive out of order. The policy layer treats them as
/// opaque values and copies them into the receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderOverride {
    /// The requested codec/backend/bitrate.
    pub preference: CodecPreference,
    /// Where the request came from (for example `controller`, `config`, `test`).
    #[serde(default = "default_source")]
    pub source: String,
    /// Request schema or payload version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Monotonic request epoch assigned by the caller.
    #[serde(default)]
    pub epoch: u64,
}

fn default_source() -> String {
    "api".to_owned()
}

fn default_version() -> u32 {
    1
}

impl EncoderOverride {
    /// Wrap a preference with default provenance: source `api`, version 1,
    /// epoch 0.
    pub fn new(preference: CodecPreference) -> Self {
        Self {
            preference,
            source: default_source(),
            version: default_version(),
            epoch: 0,
        }
    }

    /// Set the provenance source.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Set the request version.
    pub fn with_version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// Set the request epoch.
    pub fn with_epoch(mut self, epoch: u64) -> Self {
        self.epoch = epoch;
        self
    }
}

/// Why the requested backend could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// The registry has no capability for the requested codec at all.
    RequestedCodecUnavailable,
    /// The codec exists, but not with the requested backend.
    RequestedBackendUnavailable,
    /// The `(codec, backend)` pair exists but failed its probe or cannot meet
    /// the requested resolution/frame rate/HDR state.
    RequestedEncoderProbeFailed,
}

impl FallbackReason {
    /// Stable snake_case name used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestedCodecUnavailable => "requested_codec_unavailable",
            Self::RequestedBackendUnavailable => "requested_backend_unavailable",
            Self::RequestedEncoderProbeFailed => "requested_encoder_probe_failed",
        }
    }
}

/// The outcome of evaluating an override against a registry.
///
/// `selected` is always the capability the registry actually chose, never a
/// synthesised one. `applied` is `true` only when the requested codec and (if
/// pinned) backend were both honoured. It says nothing about whether a live
/// pipeline has restarted; that is the node's job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideResult {
    /// Whether the preference was honoured exactly.
    pub applied: bool,
    /// The real capability chosen, or `None` when nothing was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<EncodeCapability>,
    /// Why a different capability was chosen, when `applied` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<FallbackReason>,
}

impl OverrideResult {
    /// Build a result from a resolved selection.
    pub fn from_selection(
        selection: &EncoderSelection,
        applied: bool,
        fallback_reason: Option<FallbackReason>,
    ) -> Self {
        Self {
            applied,
            selected: Some(selection.capability.clone()),
            fallback_reason,
        }
    }
}

/// A pure record of one evaluated override.
///
/// Nodes store this alongside the active pipeline generation and can expose it
/// on `GET /codec`. `requested_format` is the effective format: the caller's
/// dimensions/frame rate/HDR with the preference's codec and bitrate applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideReceipt {
    /// Provenance from [`EncoderOverride::source`].
    pub source: String,
    /// Provenance from [`EncoderOverride::version`].
    pub version: u32,
    /// Provenance from [`EncoderOverride::epoch`].
    pub epoch: u64,
    /// The preference that was evaluated.
    pub requested: CodecPreference,
    /// The effective format selection ran against.
    pub requested_format: MediaFormat,
    /// Whether the preference was honoured exactly.
    pub applied: bool,
    /// The real capability chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<EncodeCapability>,
    /// Why a fallback capability was chosen, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<FallbackReason>,
}

impl OverrideReceipt {
    /// Build a receipt from an override, its effective format, and a result.
    pub fn new(
        request: &EncoderOverride,
        requested_format: MediaFormat,
        result: &OverrideResult,
    ) -> Self {
        Self {
            source: request.source.clone(),
            version: request.version,
            epoch: request.epoch,
            requested: request.preference.clone(),
            requested_format,
            applied: result.applied,
            selected: result.selected.clone(),
            fallback_reason: result.fallback_reason,
        }
    }
}

/// Storable codec override state for `GET /codec`.
///
/// Serialises as `{"override": <preference+provenance>|null, "receipt": {...}|null}`.
/// The node adds `current_encoder`/`available` from its live pipeline and
/// capabilities; this type only carries what the policy evaluated.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecOverrideState {
    /// The accepted override, or `None` when the node is on automatic selection.
    #[serde(rename = "override")]
    pub requested: Option<EncoderOverride>,
    /// The last evaluated receipt, or `None` when no override was evaluated.
    pub receipt: Option<OverrideReceipt>,
}

impl CodecOverrideState {
    /// No override and no receipt.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build state for an accepted override and its receipt.
    pub fn from_receipt(request: EncoderOverride, receipt: OverrideReceipt) -> Self {
        Self {
            requested: Some(request),
            receipt: Some(receipt),
        }
    }
}

struct ResolvedOverride {
    selection: EncoderSelection,
    applied: bool,
    fallback_reason: Option<FallbackReason>,
}

impl EncoderRegistry {
    /// Select an encoder honouring `preference`.
    ///
    /// This is the override-aware form of [`EncoderRegistry::select`]: the
    /// effective format is `input` with the preference's codec and optional
    /// bitrate applied, so dimensions, frame rate, and HDR come from the
    /// caller. Call [`EncoderSelection::build_encoder`] on the result to get a
    /// concrete [`crate::SelectedEncoder`].
    ///
    /// # Errors
    ///
    /// * `invalid_request` for a zero bitrate or a codec/backend mismatch.
    /// * `encoder.unavailable` when the requested codec or backend is not
    ///   advertised and software fallback is not allowed (or not possible).
    /// * `encoder.probe_failed` when the requested pair exists but failed its
    ///   probe or cannot meet the format; this error is retryable.
    pub fn select_with_override(
        &self,
        preference: &CodecPreference,
        input: &MediaFormat,
    ) -> Result<EncoderSelection, ApiError> {
        self.resolve_override(preference, input)
            .map(|resolved| resolved.selection)
    }

    /// Evaluate an override and report whether it was honoured exactly.
    ///
    /// The selected capability is always the real one. A software fallback is
    /// visible as `applied: false` plus a [`FallbackReason`].
    pub fn evaluate_override(
        &self,
        request: &EncoderOverride,
        input: &MediaFormat,
    ) -> Result<OverrideResult, ApiError> {
        let resolved = self.resolve_override(&request.preference, input)?;
        Ok(OverrideResult::from_selection(
            &resolved.selection,
            resolved.applied,
            resolved.fallback_reason,
        ))
    }

    /// Build a pure [`OverrideReceipt`] for storage and `GET /codec`.
    pub fn override_receipt(
        &self,
        request: &EncoderOverride,
        input: &MediaFormat,
    ) -> Result<OverrideReceipt, ApiError> {
        let effective = request.preference.effective_format(input);
        let resolved = self.resolve_override(&request.preference, input)?;
        let result = OverrideResult::from_selection(
            &resolved.selection,
            resolved.applied,
            resolved.fallback_reason,
        );
        Ok(OverrideReceipt::new(request, effective, &result))
    }

    /// Build storable override state.
    ///
    /// `None` returns [`CodecOverrideState::empty`] with null fields. `Some`
    /// evaluates the override and returns an error when it cannot be honoured,
    /// so a node never stores a receipt for a selection that does not exist.
    pub fn codec_state(
        &self,
        request: Option<&EncoderOverride>,
        input: &MediaFormat,
    ) -> Result<CodecOverrideState, ApiError> {
        match request {
            None => Ok(CodecOverrideState::empty()),
            Some(request) => {
                let receipt = self.override_receipt(request, input)?;
                Ok(CodecOverrideState::from_receipt(request.clone(), receipt))
            }
        }
    }

    fn resolve_override(
        &self,
        preference: &CodecPreference,
        input: &MediaFormat,
    ) -> Result<ResolvedOverride, ApiError> {
        preference.validate()?;
        let effective = preference.effective_format(input);

        let Some(backend) = preference.backend else {
            // No backend pinned: use the normal hardware-first policy, with
            // software fallback controlled by the override.
            let policy = SelectionPolicy {
                allow_software_fallback: preference.allow_software_fallback,
                require_probe: false,
            };
            let selection = self
                .select_with(&effective, policy)
                .map_err(|error| self.enrich_error(error, preference, &effective))?;
            return Ok(ResolvedOverride {
                selection,
                applied: true,
                fallback_reason: None,
            });
        };

        if let Some(capability) = self.eligible_capability(preference.codec, backend, &effective) {
            let selection =
                self.selection_for(effective, capability, SelectionReason::OverrideRequested);
            return Ok(ResolvedOverride {
                selection,
                applied: true,
                fallback_reason: None,
            });
        }

        let reason = self.unavailability_reason(preference.codec, backend);
        let fallback_attempted = preference.allow_software_fallback && backend_is_hardware(backend);

        if fallback_attempted {
            if let Some(capability) =
                self.eligible_capability(preference.codec, EncoderBackend::Software, &effective)
            {
                let selection =
                    self.selection_for(effective, capability, SelectionReason::SoftwareFallback);
                return Ok(ResolvedOverride {
                    selection,
                    applied: false,
                    fallback_reason: Some(reason),
                });
            }
        }

        Err(self.override_unavailable_error(preference, &effective, reason, fallback_attempted))
    }

    fn eligible_capability(
        &self,
        codec: Codec,
        backend: EncoderBackend,
        requested: &MediaFormat,
    ) -> Option<&EncodeCapability> {
        self.capabilities().iter().find(|capability| {
            capability.codec == codec
                && capability.backend == backend
                && format_supported(capability, requested)
                && !matches!(
                    self.probe_result(capability.codec, capability.backend),
                    Some(result) if !result.is_ok()
                )
        })
    }

    fn selection_for(
        &self,
        requested: MediaFormat,
        capability: &EncodeCapability,
        reason: SelectionReason,
    ) -> EncoderSelection {
        EncoderSelection {
            requested,
            capability: capability.clone(),
            reason,
            probe: self
                .probe_result(capability.codec, capability.backend)
                .cloned(),
        }
    }

    fn unavailability_reason(&self, codec: Codec, backend: EncoderBackend) -> FallbackReason {
        if !self.has_codec(codec) {
            FallbackReason::RequestedCodecUnavailable
        } else if !self
            .capabilities()
            .iter()
            .any(|capability| capability.codec == codec && capability.backend == backend)
        {
            FallbackReason::RequestedBackendUnavailable
        } else {
            FallbackReason::RequestedEncoderProbeFailed
        }
    }

    fn override_unavailable_error(
        &self,
        preference: &CodecPreference,
        effective: &MediaFormat,
        reason: FallbackReason,
        fallback_attempted: bool,
    ) -> ApiError {
        let backend_label = preference.backend.map(backend_name).unwrap_or("auto");
        let message = match reason {
            FallbackReason::RequestedCodecUnavailable => format!(
                "No encoder on this node supports the requested codec `{}`.",
                preference.codec.as_str()
            ),
            FallbackReason::RequestedBackendUnavailable => format!(
                "The requested `{}` backend is not available for codec `{}` on this node.",
                backend_label,
                preference.codec.as_str()
            ),
            FallbackReason::RequestedEncoderProbeFailed => format!(
                "The requested `{}`/`{}` encoder is advertised but cannot satisfy {} or failed its probe.",
                preference.codec.as_str(),
                backend_label,
                effective.label()
            ),
        };

        let code = match reason {
            FallbackReason::RequestedEncoderProbeFailed => codes::ENCODER_PROBE_FAILED,
            FallbackReason::RequestedCodecUnavailable
            | FallbackReason::RequestedBackendUnavailable => codes::ENCODER_UNAVAILABLE,
        };

        let remediation = if fallback_attempted {
            "Install or enable the requested encoder, or allow the software fallback."
        } else {
            "Choose an advertised codec/backend, or request the codec without a backend and allow the software fallback."
        };

        let error = ApiError::new(code, message)
            .with_details(self.override_details(preference, effective, reason))
            .with_remediation(remediation);

        if reason == FallbackReason::RequestedEncoderProbeFailed {
            error.retryable()
        } else {
            error
        }
    }

    fn override_details(
        &self,
        preference: &CodecPreference,
        effective: &MediaFormat,
        reason: FallbackReason,
    ) -> Value {
        json!({
            "requested_codec": preference.codec.as_str(),
            "requested_backend": preference.backend.map(backend_name),
            "requested_format": effective,
            "reason": reason,
            "available": self.capabilities(),
        })
    }

    fn enrich_error(
        &self,
        error: ApiError,
        preference: &CodecPreference,
        effective: &MediaFormat,
    ) -> ApiError {
        let ApiError {
            code,
            message,
            details,
            remediation,
            retryable,
        } = error;

        let mut details = match details {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                let mut map = Map::new();
                map.insert("detail".to_owned(), other);
                map
            }
        };
        details.insert(
            "requested_codec".to_owned(),
            Value::String(preference.codec.as_str().to_owned()),
        );
        if let Some(backend) = preference.backend {
            details.insert(
                "requested_backend".to_owned(),
                Value::String(backend_name(backend).to_owned()),
            );
        }
        details.insert("requested_format".to_owned(), json!(effective));
        details.insert(
            "available".to_owned(),
            serde_json::to_value(self.capabilities()).unwrap_or(Value::Null),
        );

        ApiError {
            code,
            message,
            details: Value::Object(details),
            remediation,
            retryable,
        }
    }
}

fn format_supported(capability: &EncodeCapability, requested: &MediaFormat) -> bool {
    capability.max_width >= requested.width
        && capability.max_height >= requested.height
        && (requested.fps == 0 || capability.max_fps >= requested.fps)
        && (!(requested.hdr || requested.color.is_hdr()) || capability.hdr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::FakeProbe;

    fn capability(
        codec: Codec,
        backend: EncoderBackend,
        hardware: bool,
        width: u32,
        height: u32,
        fps: u32,
    ) -> EncodeCapability {
        EncodeCapability {
            codec,
            backend,
            hardware,
            max_width: width,
            max_height: height,
            max_fps: fps,
            hdr: false,
            profiles: Vec::new(),
        }
    }

    fn h264_software() -> EncodeCapability {
        capability(Codec::H264, EncoderBackend::Software, false, 1920, 1080, 30)
    }

    fn h264_vaapi() -> EncodeCapability {
        capability(Codec::H264, EncoderBackend::Vaapi, true, 3840, 2160, 60)
    }

    fn request() -> MediaFormat {
        MediaFormat::new(Codec::H264, 1920, 1080, 30, 8_000)
    }

    #[test]
    fn explicit_backend_is_selected_when_available() {
        let registry = EncoderRegistry::from_capabilities([h264_software(), h264_vaapi()]);
        let preference = CodecPreference::new(Codec::H264)
            .with_backend(EncoderBackend::Vaapi)
            .with_bitrate_kbps(4_000);

        let selection = registry
            .select_with_override(&preference, &request())
            .unwrap();
        assert_eq!(selection.backend(), EncoderBackend::Vaapi);
        assert!(selection.is_hardware());
        assert_eq!(selection.reason, SelectionReason::OverrideRequested);
        assert_eq!(selection.requested.bitrate_kbps, 4_000);

        let result = registry
            .evaluate_override(&EncoderOverride::new(preference), &request())
            .unwrap();
        assert!(result.applied);
        assert_eq!(
            result.selected.as_ref().unwrap().backend,
            EncoderBackend::Vaapi
        );
        assert_eq!(result.fallback_reason, None);
    }

    #[test]
    fn override_selection_builds_the_real_software_encoder() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Software);

        let selection = registry
            .select_with_override(&preference, &request())
            .unwrap();
        let encoder = selection
            .build_encoder(ozma_media::FrameFormat::rgb24(320, 240))
            .unwrap();
        assert!(matches!(encoder, crate::SelectedEncoder::Software(_)));
    }

    #[test]
    fn explicit_software_is_honoured_without_fallback_flag() {
        let registry = EncoderRegistry::from_capabilities([h264_software(), h264_vaapi()]);
        let preference = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Software);

        let result = registry
            .evaluate_override(&EncoderOverride::new(preference), &request())
            .unwrap();
        assert!(result.applied);
        assert_eq!(
            result.selected.as_ref().unwrap().backend,
            EncoderBackend::Software
        );
        assert_eq!(result.fallback_reason, None);
    }

    #[test]
    fn unavailable_backend_errors_with_real_capabilities() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Vaapi);

        let error = registry
            .select_with_override(&preference, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
        assert_eq!(error.details["requested_backend"], "vaapi");
        assert_eq!(error.details["reason"], "requested_backend_unavailable");

        let available = error.details["available"].as_array().unwrap();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0]["codec"], "h264");
        assert_eq!(available[0]["backend"], "software");
        assert_eq!(available[0]["hardware"], false);
    }

    #[test]
    fn unavailable_codec_errors_and_lists_what_exists() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::Av1);

        let error = registry
            .select_with_override(&preference, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
        assert_eq!(error.details["requested_codec"], "av1");
        assert_eq!(error.details["available"][0]["codec"], "h264");
    }

    #[test]
    fn software_fallback_is_used_only_when_allowed() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);

        let denied = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Vaapi);
        let error = registry
            .select_with_override(&denied, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);

        let allowed = denied.with_software_fallback(true);
        let result = registry
            .evaluate_override(&EncoderOverride::new(allowed), &request())
            .unwrap();
        assert!(!result.applied);
        assert_eq!(
            result.fallback_reason,
            Some(FallbackReason::RequestedBackendUnavailable)
        );
        assert_eq!(
            result.selected.as_ref().unwrap().backend,
            EncoderBackend::Software
        );
    }

    #[test]
    fn a_software_result_never_claims_hardware() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264)
            .with_backend(EncoderBackend::Vaapi)
            .with_software_fallback(true);

        let selection = registry
            .select_with_override(&preference, &request())
            .unwrap();
        assert!(!selection.is_hardware());
        assert_eq!(selection.capability.backend, EncoderBackend::Software);
        assert!(!selection.capability.hardware);

        let result = registry
            .evaluate_override(&EncoderOverride::new(preference), &request())
            .unwrap();
        let selected = result.selected.unwrap();
        assert_eq!(selected.backend, EncoderBackend::Software);
        assert!(!selected.hardware);
    }

    #[tokio::test]
    async fn probe_failure_is_reported_and_can_fall_back() {
        let mut registry = EncoderRegistry::from_capabilities([h264_vaapi(), h264_software()]);
        registry
            .probe_all(&FakeProbe::passing().fail_for(Codec::H264, EncoderBackend::Vaapi))
            .await;

        let denied = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Vaapi);
        let error = registry
            .select_with_override(&denied, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_PROBE_FAILED);
        assert!(error.retryable);
        assert_eq!(error.details["reason"], "requested_encoder_probe_failed");

        let allowed = denied.with_software_fallback(true);
        let result = registry
            .evaluate_override(&EncoderOverride::new(allowed), &request())
            .unwrap();
        assert!(!result.applied);
        assert_eq!(
            result.fallback_reason,
            Some(FallbackReason::RequestedEncoderProbeFailed)
        );
        assert_eq!(result.selected.unwrap().backend, EncoderBackend::Software);
    }

    #[test]
    fn zero_bitrate_is_invalid() {
        let preference = CodecPreference::new(Codec::H264).with_bitrate_kbps(0);
        let error = preference.validate().unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert_eq!(error.details["bitrate_kbps"], 0);

        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let error = registry
            .select_with_override(&preference, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn bitrate_override_reaches_the_effective_format() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264)
            .with_backend(EncoderBackend::Software)
            .with_bitrate_kbps(2_500);
        let override_request = EncoderOverride::new(preference)
            .with_source("controller")
            .with_version(3)
            .with_epoch(42);

        let receipt = registry
            .override_receipt(&override_request, &request())
            .unwrap();
        assert_eq!(receipt.requested_format.codec, Codec::H264);
        assert_eq!(receipt.requested_format.bitrate_kbps, 2_500);
        assert_eq!(receipt.requested_format.width, 1920);
        assert!(receipt.applied);
    }

    #[test]
    fn receipt_state_round_trips_through_json() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264)
            .with_backend(EncoderBackend::Software)
            .with_bitrate_kbps(1_500);
        let override_request = EncoderOverride::new(preference)
            .with_source("controller")
            .with_version(3)
            .with_epoch(42);

        let state = registry
            .codec_state(Some(&override_request), &request())
            .unwrap();
        assert_eq!(state.requested.as_ref().unwrap().source, "controller");

        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["override"]["source"], "controller");
        assert_eq!(json["override"]["version"], 3);
        assert_eq!(json["override"]["epoch"], 42);
        assert_eq!(json["override"]["preference"]["codec"], "h264");
        assert_eq!(json["receipt"]["selected"]["backend"], "software");
        assert_eq!(json["receipt"]["requested_format"]["bitrate_kbps"], 1_500);
        assert_eq!(json["receipt"]["applied"], true);

        let round_trip: CodecOverrideState = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, state);

        let empty = registry.codec_state(None, &request()).unwrap();
        assert_eq!(empty, CodecOverrideState::default());
        let empty_json = serde_json::to_value(&empty).unwrap();
        assert!(empty_json["override"].is_null());
        assert!(empty_json["receipt"].is_null());
    }

    #[test]
    fn preference_deserializes_with_strict_defaults() {
        let preference: CodecPreference =
            serde_json::from_value(json!({ "codec": "h264" })).unwrap();
        assert_eq!(preference.backend, None);
        assert_eq!(preference.bitrate_kbps, None);
        assert!(!preference.allow_software_fallback);

        let preference: CodecPreference = serde_json::from_value(json!({
            "codec": "hevc",
            "backend": "vaapi",
            "bitrate_kbps": 6_000,
            "allow_software_fallback": true,
        }))
        .unwrap();
        assert_eq!(preference.codec, Codec::Hevc);
        assert_eq!(preference.backend, Some(EncoderBackend::Vaapi));
        assert_eq!(preference.bitrate_kbps, Some(6_000));
        assert!(preference.allow_software_fallback);
    }

    #[test]
    fn fake_capability_is_selectable_but_never_hardware() {
        let registry = EncoderRegistry::from_capabilities([capability(
            Codec::H264,
            EncoderBackend::Fake,
            false,
            1920,
            1080,
            30,
        )]);

        let preference = CodecPreference::new(Codec::H264).with_backend(EncoderBackend::Fake);
        let result = registry
            .evaluate_override(&EncoderOverride::new(preference), &request())
            .unwrap();
        assert!(result.applied);
        let selected = result.selected.unwrap();
        assert_eq!(selected.backend, EncoderBackend::Fake);
        assert!(!selected.hardware);

        // A hardware request never falls back into the fake backend.
        let preference = CodecPreference::new(Codec::H264)
            .with_backend(EncoderBackend::Vaapi)
            .with_software_fallback(true);
        let error = registry
            .select_with_override(&preference, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
        assert_eq!(
            error.details["available"][0]["backend"], "fake",
            "the error must only advertise what really exists"
        );
    }

    #[test]
    fn hardware_backend_for_audio_codec_is_invalid() {
        let preference = CodecPreference::new(Codec::Opus).with_backend(EncoderBackend::Vaapi);
        let error = preference.validate().unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
        assert_eq!(error.details["backend"], "vaapi");

        let software = CodecPreference::new(Codec::Opus).with_backend(EncoderBackend::Software);
        assert!(software.validate().is_ok());
    }

    #[test]
    fn codec_only_preference_matches_plain_selection() {
        let registry = EncoderRegistry::from_capabilities([h264_software(), h264_vaapi()]);
        let plain = registry.select(&request()).unwrap();
        let preference = CodecPreference::new(Codec::H264);
        let overridden = registry
            .select_with_override(&preference, &request())
            .unwrap();
        assert_eq!(overridden, plain);
    }

    #[test]
    fn codec_only_preference_honours_the_fallback_flag() {
        let registry = EncoderRegistry::from_capabilities([h264_software()]);
        let preference = CodecPreference::new(Codec::H264);

        let error = registry
            .select_with_override(&preference, &request())
            .unwrap_err();
        assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);

        let allowed = preference.with_software_fallback(true);
        let result = registry
            .evaluate_override(&EncoderOverride::new(allowed), &request())
            .unwrap();
        assert!(result.applied);
        assert_eq!(result.selected.unwrap().backend, EncoderBackend::Software);
    }
}
