// SPDX-License-Identifier: AGPL-3.0-only
//! Encoder capability selection with hardware preference and software fallback.
//!
//! Selection is intentionally deterministic and explainable: it filters by
//! codec, dimensions, frame rate, HDR requirement, and recorded probe failures,
//! then chooses the best hardware candidate. Software is used only when no
//! hardware candidate is eligible, and the result says so.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use ozma_media::{EncodedFrame, Encoder, FakeEncoder, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, Codec, EncodeCapability, EncoderBackend, MediaFormat};
use tokio::process::Command as ProcessCommand;

use crate::probe::{EncoderProbe, ProbeResult};
use crate::software::H264SoftwareEncoder;
use crate::vaapi::{detect_vaapi_device, ffmpeg_binary, unix_now, VaapiH264Encoder, VaapiProbe};

/// Largest envelope advertised for a detected hardware encoder that has no
/// device-specific 4K evidence yet.
const DETECTED_HARDWARE_MAX_WIDTH: u32 = 1920;
/// Largest height advertised for a detected hardware encoder.
const DETECTED_HARDWARE_MAX_HEIGHT: u32 = 1080;
/// Highest frame rate advertised for a detected hardware encoder.
const DETECTED_HARDWARE_MAX_FPS: u32 = 60;

/// How long `ffmpeg -encoders` may take during detection.
const ENCODER_LIST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a single one-frame QSV/V4L2M2M availability probe may take.
const ENCODER_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Hardware encoders that are only advertised after a real encode check.
///
/// VAAPI has its own richer probe ([`VaapiProbe`]) that also validates the
/// access unit. NVENC is not in this set: a `/dev/nvidia*` device plus the
/// ffmpeg listing is enough to advertise it, because the HLS path
/// runtime-probes the exact argv before applying it.

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
    /// A caller pinned this exact backend through
    /// [`crate::CodecPreference`].
    OverrideRequested,
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

/// Host facts used to decide which real encoders may be advertised.
///
/// This is deliberately a plain value: production fills it with
/// [`EncoderEnvironment::detect`], while tests construct it directly so no
/// GPU, driver, or ffmpeg process is needed. A backend is only advertised when
/// its required device fact is present; an encoder name alone never proves a
/// device exists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncoderEnvironment {
    /// The VAAPI render node that exists and should be probed, if any.
    ///
    /// A configured `--vaapi-device` that does not exist is represented as
    /// `None`; detection never silently falls back to a different node.
    pub vaapi_device: Option<PathBuf>,
    /// Exact names reported by `ffmpeg -encoders`.
    pub ffmpeg_encoders: BTreeSet<String>,
    /// A CUDA/NVML device node is present.
    pub nvidia_device: bool,
    /// Hardware encoders that passed a real one-frame encode check.
    ///
    /// This is used for QSV and V4L2M2M, where `ffmpeg -encoders` plus a device
    /// node is not enough evidence: a wrapper can be compiled in while the
    /// driver/device still refuses every session.
    pub verified_encoders: BTreeSet<String>,
}

impl EncoderEnvironment {
    /// An empty environment: no VAAPI, NVIDIA, or verified encoders.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an existing VAAPI render node.
    pub fn with_vaapi_device(mut self, device: impl Into<PathBuf>) -> Self {
        self.vaapi_device = Some(device.into());
        self
    }

    /// Record exact ffmpeg encoder names.
    pub fn with_ffmpeg_encoders(
        mut self,
        encoders: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.ffmpeg_encoders = encoders.into_iter().map(Into::into).collect();
        self
    }

    /// Record whether a CUDA/NVML device node is present.
    pub fn with_nvidia_device(mut self, present: bool) -> Self {
        self.nvidia_device = present;
        self
    }

    /// Record a hardware encoder that passed a real encode check.
    pub fn with_verified_encoder(mut self, encoder: impl Into<String>) -> Self {
        self.verified_encoders.insert(encoder.into());
        self
    }

    /// True when ffmpeg advertised `name`.
    pub fn has_ffmpeg_encoder(&self, name: &str) -> bool {
        self.ffmpeg_encoders.contains(name)
    }

    /// True when a real encode check proved `name` works.
    pub fn has_verified_encoder(&self, name: &str) -> bool {
        self.verified_encoders.contains(name)
    }

    /// Detect the real host environment for `vaapi_device`.
    ///
    /// Only the configured VAAPI path is accepted when one is supplied; an
    /// absent path records no VAAPI device rather than guessing another render
    /// node. QSV/V4L2M2M run a bounded one-frame ffmpeg encode because their
    /// mere presence in `ffmpeg -encoders` is not evidence that the device
    /// works. NVENC deliberately does not run a full encode here; the HLS path
    /// runtime-probes the exact argv before applying it.
    pub async fn detect(vaapi_device: Option<&Path>) -> Self {
        let vaapi_device = resolve_vaapi_device(vaapi_device);
        let ffmpeg_encoders = ffmpeg_encoder_names().await;
        let nvidia_device = nvidia_device_present();

        let (qsv_h264, qsv_hevc, v4l2_h264, v4l2_hevc) = tokio::join!(
            ffmpeg_encoder_encodes_if_listed(&ffmpeg_encoders, "h264_qsv"),
            ffmpeg_encoder_encodes_if_listed(&ffmpeg_encoders, "hevc_qsv"),
            ffmpeg_encoder_encodes_if_listed(&ffmpeg_encoders, "h264_v4l2m2m"),
            ffmpeg_encoder_encodes_if_listed(&ffmpeg_encoders, "hevc_v4l2m2m"),
        );

        let mut verified_encoders = BTreeSet::new();
        for (encoder, verified) in [
            ("h264_qsv", qsv_h264),
            ("hevc_qsv", qsv_hevc),
            ("h264_v4l2m2m", v4l2_h264),
            ("hevc_v4l2m2m", v4l2_hevc),
        ] {
            if verified {
                verified_encoders.insert(encoder.to_owned());
            }
        }

        Self {
            vaapi_device,
            ffmpeg_encoders,
            nvidia_device,
            verified_encoders,
        }
    }
}

/// Resolve the VAAPI node to probe.
///
/// An explicit path is honoured only when it exists. Without one, the normal
/// `/dev/dri/renderD128`/lowest-node detection applies.
fn resolve_vaapi_device(configured: Option<&Path>) -> Option<PathBuf> {
    match configured {
        Some(path) => path.exists().then(|| path.to_path_buf()),
        None => detect_vaapi_device(),
    }
}

/// True when a CUDA/NVML device node is present.
fn nvidia_device_present() -> bool {
    Path::new("/dev/nvidiactl").exists()
        || Path::new("/dev/nvidia-uvm").exists()
        || (0..8).any(|index| Path::new(&format!("/dev/nvidia{index}")).exists())
}

/// Run `ffmpeg -encoders` once with a bounded wait.
async fn ffmpeg_encoder_names() -> BTreeSet<String> {
    let binary = ffmpeg_binary();
    let output = tokio::time::timeout(
        ENCODER_LIST_TIMEOUT,
        ProcessCommand::new(&binary)
            .args(["-hide_banner", "-loglevel", "error", "-encoders"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await;

    match output {
        Ok(Ok(output)) if output.status.success() => {
            parse_ffmpeg_encoder_names(&String::from_utf8_lossy(&output.stdout))
        }
        _ => BTreeSet::new(),
    }
}

/// Parse the encoder table printed by `ffmpeg -encoders`.
fn parse_ffmpeg_encoder_names(output: &str) -> BTreeSet<String> {
    let mut encoders = BTreeSet::new();
    let mut in_table = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("------") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let flags = fields.next().unwrap_or_default();
        if flags.len() < 5 {
            continue;
        }
        if let Some(name) = fields.next() {
            encoders.insert(name.to_owned());
        }
    }
    encoders
}

/// Try a one-frame encode only when ffmpeg advertises the wrapper.
async fn ffmpeg_encoder_encodes_if_listed(available: &BTreeSet<String>, encoder: &str) -> bool {
    available.contains(encoder) && ffmpeg_encoder_encodes(encoder).await
}

/// Try one synthetic frame through `encoder` and require a clean ffmpeg exit.
async fn ffmpeg_encoder_encodes(encoder: &str) -> bool {
    let binary = ffmpeg_binary();
    let pix_fmt = if encoder.ends_with("_qsv") {
        "nv12"
    } else {
        "yuv420p"
    };
    let status = tokio::time::timeout(
        ENCODER_CHECK_TIMEOUT,
        ProcessCommand::new(&binary)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=1",
                "-frames:v",
                "1",
                "-c:v",
                encoder,
                "-pix_fmt",
                pix_fmt,
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;

    matches!(status, Ok(Ok(status)) if status.success())
}

/// A capability for a detected hardware encoder whose device check passed.
fn detected_hardware_capability(
    codec: Codec,
    backend: EncoderBackend,
    profiles: &[&str],
) -> EncodeCapability {
    EncodeCapability {
        codec,
        backend,
        hardware: true,
        max_width: DETECTED_HARDWARE_MAX_WIDTH,
        max_height: DETECTED_HARDWARE_MAX_HEIGHT,
        max_fps: DETECTED_HARDWARE_MAX_FPS,
        hdr: false,
        profiles: profiles
            .iter()
            .map(|profile| (*profile).to_owned())
            .collect(),
    }
}

/// VAAPI HEVC uses the same device/driver evidence as the H.264 probe.
fn vaapi_hevc_capability() -> EncodeCapability {
    detected_hardware_capability(Codec::Hevc, EncoderBackend::Vaapi, &["main"])
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

    /// A registry with every encoder this host can honestly advertise.
    ///
    /// The software H.264 fallback is always present. Hardware is added only
    /// after its required evidence:
    ///
    /// * VAAPI H.264 after a real [`VaapiProbe`] encode on the configured
    ///   render node; VAAPI HEVC is added when that device probe passes and
    ///   ffmpeg also lists `hevc_vaapi`.
    /// * NVENC H.264/HEVC when ffmpeg lists the wrapper and a CUDA/NVML device
    ///   node exists. The HLS path runtime-probes the exact argv before it
    ///   reports success.
    /// * QSV/V4L2M2M only when the name is listed **and** a real one-frame
    ///   encode check passed.
    ///
    /// This uses the normal `/dev/dri` detection. Call
    /// [`EncoderRegistry::with_detected_encoders_for_device`] when the node has
    /// an explicit `--vaapi-device`.
    pub async fn with_detected_encoders() -> Self {
        Self::with_detected_encoders_for_device(None).await
    }

    /// A registry detected against the configured VAAPI render node.
    ///
    /// `None` keeps the normal `/dev/dri/renderD128`/lowest-render-node
    /// detection. `Some(path)` is honoured only when the path exists; an
    /// absent configured path does not silently probe a different device.
    pub async fn with_detected_encoders_for_device(device: Option<&Path>) -> Self {
        let environment = EncoderEnvironment::detect(device).await;
        Self::with_detected_encoders_in(&environment).await
    }

    /// A registry built from an already-detected host environment.
    ///
    /// Production uses this after [`EncoderEnvironment::detect`]; tests can
    /// build a fake environment without touching hardware.
    pub async fn with_detected_encoders_in(environment: &EncoderEnvironment) -> Self {
        let probe = match environment.vaapi_device.as_deref() {
            Some(device) => VaapiProbe::new().with_device(device),
            None => VaapiProbe::new(),
        };
        Self::with_detected_encoders_and_probe(environment, &probe).await
    }

    /// A registry built from an environment plus an injected VAAPI probe.
    ///
    /// The probe is only consulted when the environment records an existing
    /// VAAPI device, so a fake passing probe cannot invent hardware on a host
    /// with no render node.
    pub async fn with_detected_encoders_and_probe(
        environment: &EncoderEnvironment,
        vaapi_probe: &dyn EncoderProbe,
    ) -> Self {
        let mut registry = Self::with_software_h264();

        if environment.vaapi_device.is_some() {
            let capability = VaapiH264Encoder::capability();
            let result = vaapi_probe.probe(&capability).await;
            let ok = result.is_ok();
            registry.register_probe(result);
            if ok {
                registry.register(capability);
                // The device is proven by the H.264 encode. HEVC uses the same
                // node/driver; HLS runtime-probes the actual HEVC argv before
                // applying it.
                if environment.has_ffmpeg_encoder("hevc_vaapi") {
                    registry.register(vaapi_hevc_capability());
                }
            }
        }

        if environment.nvidia_device {
            if environment.has_ffmpeg_encoder("h264_nvenc") {
                registry.register(detected_hardware_capability(
                    Codec::H264,
                    EncoderBackend::Nvenc,
                    &["high"],
                ));
            }
            if environment.has_ffmpeg_encoder("hevc_nvenc") {
                registry.register(detected_hardware_capability(
                    Codec::Hevc,
                    EncoderBackend::Nvenc,
                    &["main"],
                ));
            }
        }

        for (name, codec, backend) in [
            ("h264_qsv", Codec::H264, EncoderBackend::Qsv),
            ("hevc_qsv", Codec::Hevc, EncoderBackend::Qsv),
            ("h264_v4l2m2m", Codec::H264, EncoderBackend::V4l2M2m),
            ("hevc_v4l2m2m", Codec::Hevc, EncoderBackend::V4l2M2m),
        ] {
            if environment.has_ffmpeg_encoder(name) && environment.has_verified_encoder(name) {
                let profiles: &[&str] = if codec == Codec::H264 {
                    &["high"]
                } else {
                    &["main"]
                };
                registry.register(detected_hardware_capability(codec, backend, profiles));
            }
        }

        registry
    }

    /// Probe VAAPI on the default render node and advertise it only when the
    /// probe passed.
    ///
    /// This is kept for callers that only need the VAAPI H.264 decision.
    /// Device-aware detection should use
    /// [`EncoderRegistry::with_detected_encoders_for_device`].
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

    /// Choose the best encoder this crate can actually construct.
    ///
    /// [`EncoderSelection::build_encoder`] only implements software, VAAPI, and
    /// fake backends. NVENC, QSV, and V4L2M2M are advertised so the HLS
    /// controller can select and runtime-probe them, but a caller that needs a
    /// concrete `ozma-encode` encoder (the session/native-media path) must use
    /// this method so it does not select a backend `build_encoder` would reject.
    pub fn select_buildable(&self, requested: &MediaFormat) -> Result<EncoderSelection, ApiError> {
        let buildable = Self {
            capabilities: self
                .capabilities
                .iter()
                .filter(|capability| {
                    matches!(
                        capability.backend,
                        EncoderBackend::Software | EncoderBackend::Vaapi | EncoderBackend::Fake
                    )
                })
                .cloned()
                .collect(),
            probes: self.probes.clone(),
        };
        buildable.select(requested)
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

    #[test]
    fn configured_vaapi_device_is_used_only_when_it_exists() {
        let directory =
            std::env::temp_dir().join(format!("ozma-encode-vaapi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create temp dir");
        let device = directory.join("renderD129");
        std::fs::write(&device, b"").expect("fake VAAPI device");

        assert_eq!(resolve_vaapi_device(Some(&device)), Some(device.clone()));
        assert_eq!(
            resolve_vaapi_device(Some(&directory.join("renderD999"))),
            None,
            "a missing configured device must not fall back to another render node"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    fn has(registry: &EncoderRegistry, codec: Codec, backend: EncoderBackend) -> bool {
        registry
            .capabilities()
            .iter()
            .any(|capability| capability.codec == codec && capability.backend == backend)
    }

    #[tokio::test]
    async fn device_probe_advertises_vaapi_h264_and_listed_hevc() {
        let environment = EncoderEnvironment::new()
            .with_vaapi_device("/dev/dri/renderD129")
            .with_ffmpeg_encoders(["hevc_vaapi"]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&environment, &FakeProbe::passing())
                .await;

        assert!(has(&registry, Codec::H264, EncoderBackend::Vaapi));
        assert!(has(&registry, Codec::Hevc, EncoderBackend::Vaapi));
        assert!(has(&registry, Codec::H264, EncoderBackend::Software));
        assert!(
            registry
                .probe_result(Codec::H264, EncoderBackend::Vaapi)
                .is_some_and(ProbeResult::is_ok),
            "the device probe is the evidence for VAAPI H.264"
        );

        let hevc = MediaFormat::new(Codec::Hevc, 1920, 1080, 30, 8_000);
        let selection = registry.select(&hevc).unwrap();
        assert_eq!(selection.backend(), EncoderBackend::Vaapi);
        assert!(selection.is_hardware());
    }

    #[tokio::test]
    async fn no_device_or_a_failed_probe_never_advertises_vaapi() {
        let no_device = EncoderEnvironment::new().with_ffmpeg_encoders(["hevc_vaapi"]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&no_device, &FakeProbe::passing())
                .await;
        assert!(!has(&registry, Codec::H264, EncoderBackend::Vaapi));
        assert!(!has(&registry, Codec::Hevc, EncoderBackend::Vaapi));
        assert!(
            registry
                .probe_result(Codec::H264, EncoderBackend::Vaapi)
                .is_none(),
            "a probe must not run without a device"
        );

        let failed_device = EncoderEnvironment::new()
            .with_vaapi_device("/dev/dri/renderD129")
            .with_ffmpeg_encoders(["hevc_vaapi"]);
        let registry = EncoderRegistry::with_detected_encoders_and_probe(
            &failed_device,
            &FakeProbe::failing(),
        )
        .await;
        assert!(!has(&registry, Codec::H264, EncoderBackend::Vaapi));
        assert!(!has(&registry, Codec::Hevc, EncoderBackend::Vaapi));
        assert!(!registry
            .probe_result(Codec::H264, EncoderBackend::Vaapi)
            .unwrap()
            .is_ok());
        assert_eq!(
            registry.select(&h264_request()).unwrap().backend(),
            EncoderBackend::Software
        );
    }

    #[tokio::test]
    async fn vaapi_hevc_requires_the_ffmpeg_listing() {
        let environment = EncoderEnvironment::new().with_vaapi_device("/dev/dri/renderD129");
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&environment, &FakeProbe::passing())
                .await;

        assert!(has(&registry, Codec::H264, EncoderBackend::Vaapi));
        assert!(
            !has(&registry, Codec::Hevc, EncoderBackend::Vaapi),
            "hevc_vaapi must be listed by ffmpeg before HEVC is advertised"
        );
    }

    #[tokio::test]
    async fn nvenc_requires_a_listed_encoder_and_an_nvidia_device() {
        let both = EncoderEnvironment::new()
            .with_nvidia_device(true)
            .with_ffmpeg_encoders(["h264_nvenc", "hevc_nvenc"]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&both, &FakeProbe::passing()).await;
        assert!(has(&registry, Codec::H264, EncoderBackend::Nvenc));
        assert!(has(&registry, Codec::Hevc, EncoderBackend::Nvenc));

        let no_device = EncoderEnvironment::new()
            .with_nvidia_device(false)
            .with_ffmpeg_encoders(["h264_nvenc", "hevc_nvenc"]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&no_device, &FakeProbe::passing())
                .await;
        assert!(!has(&registry, Codec::H264, EncoderBackend::Nvenc));
        assert!(!has(&registry, Codec::Hevc, EncoderBackend::Nvenc));

        let not_listed = EncoderEnvironment::new()
            .with_nvidia_device(true)
            .with_ffmpeg_encoders(["hevc_vaapi"]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&not_listed, &FakeProbe::passing())
                .await;
        assert!(!has(&registry, Codec::H264, EncoderBackend::Nvenc));
        assert!(!has(&registry, Codec::Hevc, EncoderBackend::Nvenc));
    }

    #[tokio::test]
    async fn qsv_and_v4l2m2m_require_a_verified_encode() {
        let names = ["h264_qsv", "hevc_qsv", "h264_v4l2m2m", "hevc_v4l2m2m"];

        let unverified = EncoderEnvironment::new().with_ffmpeg_encoders(names);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&unverified, &FakeProbe::passing())
                .await;
        for (codec, backend) in [
            (Codec::H264, EncoderBackend::Qsv),
            (Codec::Hevc, EncoderBackend::Qsv),
            (Codec::H264, EncoderBackend::V4l2M2m),
            (Codec::Hevc, EncoderBackend::V4l2M2m),
        ] {
            assert!(
                !has(&registry, codec, backend),
                "an ffmpeg listing alone must not advertise {codec:?}/{backend:?}"
            );
        }

        let verified = EncoderEnvironment::new()
            .with_ffmpeg_encoders(names)
            .with_verified_encoder("h264_qsv")
            .with_verified_encoder("hevc_qsv")
            .with_verified_encoder("h264_v4l2m2m");
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&verified, &FakeProbe::passing())
                .await;
        assert!(has(&registry, Codec::H264, EncoderBackend::Qsv));
        assert!(has(&registry, Codec::Hevc, EncoderBackend::Qsv));
        assert!(has(&registry, Codec::H264, EncoderBackend::V4l2M2m));
        assert!(!has(&registry, Codec::Hevc, EncoderBackend::V4l2M2m));
    }

    #[tokio::test]
    async fn detection_never_invents_hardware_or_fake() {
        let environment = EncoderEnvironment::new().with_ffmpeg_encoders([
            "h264_vaapi",
            "hevc_vaapi",
            "h264_nvenc",
            "hevc_nvenc",
            "h264_qsv",
            "hevc_qsv",
            "h264_v4l2m2m",
            "hevc_v4l2m2m",
        ]);
        let registry =
            EncoderRegistry::with_detected_encoders_and_probe(&environment, &FakeProbe::passing())
                .await;

        assert_eq!(
            registry.capabilities().len(),
            1,
            "only the software fallback may remain without a device"
        );
        assert!(has(&registry, Codec::H264, EncoderBackend::Software));
        assert!(registry
            .capabilities()
            .iter()
            .all(|capability| capability.backend == EncoderBackend::Software));
    }

    #[test]
    fn buildable_selection_skips_hls_only_hardware() {
        let nvenc = capability(
            Codec::H264,
            EncoderBackend::Nvenc,
            true,
            1920,
            1080,
            60,
            false,
        );
        let registry = EncoderRegistry::from_capabilities([h264_software(), nvenc]);

        let advertised = registry.select(&h264_request()).unwrap();
        assert_eq!(advertised.backend(), EncoderBackend::Nvenc);

        let buildable = registry.select_buildable(&h264_request()).unwrap();
        assert_eq!(buildable.backend(), EncoderBackend::Software);
        assert_eq!(buildable.reason, SelectionReason::OnlyCandidate);
    }

    #[test]
    fn ffmpeg_encoder_table_is_parsed_exactly() {
        let output = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC
 V....D h264_vaapi           H.264/MPEG-4 AVC (VAAPI)
 A....D aac                  AAC (Advanced Audio Coding)
";
        let parsed = parse_ffmpeg_encoder_names(output);
        assert!(parsed.contains("libx264"));
        assert!(parsed.contains("h264_vaapi"));
        assert!(parsed.contains("aac"));
        assert_eq!(parsed.len(), 3);
        assert!(!parsed.contains("Encoders:"));
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
