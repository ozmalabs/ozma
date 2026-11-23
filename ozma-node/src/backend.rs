// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Adapter from the real session manager to the API backend contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ozma_api::state::{CaptureReport, ClientCaptureSink};
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, TokenClaims, TokenIssuer, TokenVerifier};
use ozma_capture::CaptureRegistry;
use ozma_display::{FakeVirtualDisplayProvider, VirtualDisplayProvider};
use ozma_encode::{EncoderRegistry, SelectedEncoder};
use ozma_input::{FakeInputSink, InputSink, SharedSink};
use ozma_protocol::scene::SceneCommit;
use ozma_protocol::{
    transport::TransportInfo, ApiError, CheckStatus, DiagnosticCheck, DiagnosticReport, DisplayId,
    DisplayMode, DisplayTopology, DpmsState, EndReason, NodeCapabilities, PermissionSet,
    RuntimeMetrics, Session, SessionEvent, SessionId, SessionOffer, SessionSpec, SessionState,
    TransportMode, VirtualDisplaySpec,
};
use ozma_session::SessionManager;
use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
use ozma_transport::ChannelMap;
use rand::RngCore;
use tokio::sync::broadcast;

use crate::client_capture::ClientCaptureHub;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
struct TransportOffer {
    endpoint: String,
    certificate_fingerprint: String,
    certificate_der_base64: String,
}

/// The browser-facing WebTransport listener's public details.
///
/// `GET /api/v1/webtransport` returns this shape. It is additive to
/// [`TransportInfo`], whose schema is owned by `ozma-protocol` and still
/// describes the raw QUIC listener.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WebTransportInfo {
    /// `https://host:port/session`, exactly as advertised in the session offer.
    pub endpoint: String,
    /// Lowercase hex SHA-256 of the DER certificate the listener presents.
    pub fingerprint: String,
    /// Base64 DER certificate for `serverCertificateHashes` and diagnostics.
    pub certificate_der_base64: String,
}

/// An optional new resource and the placement commit that references it. Producers
/// are intentionally synchronous at this boundary. Browser and compositor
/// integrations can prepare updates off-thread and expose a snapshot here.
pub struct SceneUpdate {
    pub resource: Option<Vec<u8>>,
    pub commit: SceneCommit,
}

pub trait SceneProducer: Send + Sync {
    fn updates(&self, session: &Session) -> Vec<SceneUpdate>;

    /// Return updates newer than the last revision sent on a connection.
    /// Producers that have a live queue can override this without rebuilding
    /// the complete snapshot on every poll.
    fn updates_since(&self, session: &Session, revision: u64) -> Vec<SceneUpdate> {
        self.updates(session)
            .into_iter()
            .filter(|update| update.commit.revision > revision)
            .collect()
    }
}

/// The node's production backend: the session manager plus display state and
/// capability/diagnostic reporting.
pub struct SessionBackend {
    manager: SessionManager,
    capabilities: NodeCapabilities,
    displays: FakeVirtualDisplayProvider,
    captures: CaptureRegistry,
    encoders: EncoderRegistry,
    token_issuer: TokenIssuer,
    token_verifier: TokenVerifier,
    transport: Mutex<Option<TransportOffer>>,
    webtransport: Mutex<Option<TransportOffer>>,
    input: SharedSink,
    scene_producer: Option<Arc<dyn SceneProducer>>,
    client_captures: Mutex<HashMap<SessionId, ClientCaptureHub>>,
}

impl SessionBackend {
    pub fn new(capabilities: NodeCapabilities, consent_policy: ConsentPolicy) -> Self {
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        let displays = FakeVirtualDisplayProvider::new(capabilities.display.clone());
        Self {
            manager: SessionManager::new(consent_policy),
            capabilities,
            displays,
            captures: CaptureRegistry::system_with_pipewire(),
            encoders: EncoderRegistry::with_software_h264(),
            token_issuer: TokenIssuer::new(secret),
            token_verifier: TokenVerifier::new(secret),
            transport: Mutex::new(None),
            webtransport: Mutex::new(None),
            input: Arc::new(FakeInputSink::default()),
            scene_producer: None,
            client_captures: Mutex::new(HashMap::new()),
        }
    }

    /// The browser camera/microphone hub for `session_id`, creating it on
    /// first use.
    ///
    /// The transport and the HTTP capture sink share this handle, so decoding
    /// state does not depend on which surface touches it first.
    pub fn client_capture(&self, session_id: &SessionId) -> ClientCaptureHub {
        self.client_captures
            .lock()
            .expect("client capture registry lock")
            .entry(session_id.clone())
            .or_default()
            .clone()
    }

    /// The capture counters for an existing hub, without creating one.
    pub fn client_capture_report(&self, session_id: &SessionId) -> Option<CaptureReport> {
        self.client_captures
            .lock()
            .expect("client capture registry lock")
            .get(session_id)
            .map(ClientCaptureHub::report)
    }

    /// The latest decoded camera frame for an existing hub as PPM.
    pub fn client_capture_ppm(&self, session_id: &SessionId) -> Option<Vec<u8>> {
        self.client_captures
            .lock()
            .expect("client capture registry lock")
            .get(session_id)
            .and_then(ClientCaptureHub::latest_ppm)
    }

    /// Replace the input sink. The node uses a HID gadget sink in production
    /// and a fake/recording sink in CI and local bring-up.
    pub fn with_input_sink(mut self, sink: SharedSink) -> Self {
        self.input = sink;
        self
    }

    /// Install an optional scene producer. The default node remains video-only
    /// until a browser or compositor integration supplies one.
    pub fn with_scene_producer(mut self, producer: Arc<dyn SceneProducer>) -> Self {
        self.scene_producer = Some(producer);
        self
    }

    pub fn scene_updates(&self, session: &Session) -> Vec<SceneUpdate> {
        self.scene_producer
            .as_ref()
            .map(|producer| producer.updates(session))
            .unwrap_or_default()
    }

    pub fn scene_updates_since(&self, session: &Session, revision: u64) -> Vec<SceneUpdate> {
        self.scene_producer
            .as_ref()
            .map(|producer| producer.updates_since(session, revision))
            .unwrap_or_default()
    }

    pub fn input_sink(&self) -> &dyn InputSink {
        self.input.as_ref()
    }

    /// Replace the encoder registry with one that has been probed against the
    /// host. The advertised capabilities are updated to match.
    pub fn with_encoder_registry(mut self, encoders: EncoderRegistry) -> Self {
        self.capabilities.encode = encoders.capabilities().to_vec();
        self.encoders = encoders;
        self
    }

    /// Select and construct an encoder for a raw capture format.
    pub fn build_encoder(
        &self,
        input: ozma_media::FrameFormat,
    ) -> Result<SelectedEncoder, ApiError> {
        let request = ozma_protocol::MediaFormat::new(
            ozma_protocol::Codec::H264,
            input.width.max(2),
            input.height.max(2),
            30,
            2_000,
        );
        let selection = self.encoders.select(&request)?;
        selection.build_encoder(input)
    }

    pub fn manager(&self) -> &SessionManager {
        &self.manager
    }

    pub fn display_provider(&self) -> &dyn VirtualDisplayProvider {
        &self.displays
    }

    /// Open a capture source by id, or the first discovered source. The
    /// transport falls back to the deterministic fake source when no hardware
    /// is present.
    pub fn open_capture(
        &self,
        id: Option<&str>,
    ) -> Result<Box<dyn ozma_media::CaptureSource>, ApiError> {
        match id {
            Some(id) => self.captures.open_id(id),
            None => {
                let first = self
                    .captures
                    .enumerate()?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        ApiError::new(
                            ozma_protocol::error::codes::CAPTURE_UNAVAILABLE,
                            "No capture source is available",
                        )
                    })?;
                self.captures.open(&first)
            }
        }
    }

    /// Configure the native transport endpoint advertised in session offers.
    pub fn configure_transport(
        &self,
        endpoint: impl Into<String>,
        certificate_fingerprint: impl Into<String>,
    ) {
        self.configure_transport_with_certificate(endpoint, certificate_fingerprint, &[]);
    }

    /// Configure the native endpoint and publish the certificate clients pin.
    pub fn configure_transport_with_certificate(
        &self,
        endpoint: impl Into<String>,
        certificate_fingerprint: impl Into<String>,
        certificate_der: &[u8],
    ) {
        use base64::Engine as _;
        *self.transport.lock().expect("transport lock") = Some(TransportOffer {
            endpoint: endpoint.into(),
            certificate_fingerprint: certificate_fingerprint.into(),
            certificate_der_base64: base64::engine::general_purpose::STANDARD
                .encode(certificate_der),
        });
    }

    /// Configure the browser-facing WebTransport endpoint advertised in session
    /// offers. The URL is the complete `https://host:port/session` address.
    pub fn configure_webtransport(
        &self,
        endpoint: impl Into<String>,
        certificate_fingerprint: impl Into<String>,
    ) {
        self.configure_webtransport_with_certificate(endpoint, certificate_fingerprint, &[]);
    }

    /// Configure the WebTransport endpoint and publish the certificate the
    /// browser pins.
    pub fn configure_webtransport_with_certificate(
        &self,
        endpoint: impl Into<String>,
        certificate_fingerprint: impl Into<String>,
        certificate_der: &[u8],
    ) {
        use base64::Engine as _;
        *self.webtransport.lock().expect("webtransport lock") = Some(TransportOffer {
            endpoint: endpoint.into(),
            certificate_fingerprint: certificate_fingerprint.into(),
            certificate_der_base64: base64::engine::general_purpose::STANDARD
                .encode(certificate_der),
        });
    }

    /// The WebTransport listener's public details, if one is configured.
    pub fn webtransport_info(&self) -> Option<WebTransportInfo> {
        self.webtransport
            .lock()
            .expect("webtransport lock")
            .as_ref()
            .map(|transport| WebTransportInfo {
                endpoint: transport.endpoint.clone(),
                fingerprint: transport.certificate_fingerprint.clone(),
                certificate_der_base64: transport.certificate_der_base64.clone(),
            })
    }

    /// Verify a controller-issued session token.
    pub fn verify_token(&self, token: &str) -> Result<TokenClaims, ApiError> {
        self.token_verifier.verify(token, now())
    }

    /// Expire idle sessions and consent requests. The supervisor should call
    /// this periodically.
    pub fn expire(&self) -> usize {
        self.manager.expire(now())
    }
}

impl ClientCaptureSink for SessionBackend {
    fn capture_report(&self, session_id: &SessionId) -> Option<CaptureReport> {
        self.client_capture_report(session_id)
    }

    fn capture_frame_ppm(&self, session_id: &SessionId) -> Option<Vec<u8>> {
        self.client_capture_ppm(session_id)
    }
}

impl NodeBackend for SessionBackend {
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities.clone()
    }

    fn diagnostics(&self) -> DiagnosticReport {
        let mut checks = Vec::new();
        checks.push(DiagnosticCheck::ok("protocol", "Protocol 1.0"));
        if self.capabilities.capture.is_empty() {
            checks.push(
                DiagnosticCheck::warn("capture", "No capture source is available")
                    .with_remediation(
                        "Attach an HDMI capture device or enable the video capability.",
                    ),
            );
        } else {
            checks.push(DiagnosticCheck::ok(
                "capture",
                format!("{} capture source(s)", self.capabilities.capture.len()),
            ));
        }
        if self.capabilities.encode.is_empty() {
            checks.push(
                DiagnosticCheck::fail("encoder", "No encoder is available").with_remediation(
                    "Install a hardware encoder or enable the software fallback.",
                ),
            );
        } else {
            checks.push(DiagnosticCheck::ok(
                "encoder",
                format!("{} encoder(s)", self.capabilities.encode.len()),
            ));
        }
        if self.capabilities.display.supported {
            checks.push(DiagnosticCheck::ok(
                "display",
                "Virtual displays are supported on this platform",
            ));
        } else {
            checks.push(DiagnosticCheck::skip(
                "display",
                "Virtual displays are not supported; use a labelled mirror",
            ));
        }
        let sessions = self.manager.list();
        let active = sessions
            .iter()
            .filter(|session| session.state.is_active())
            .count();
        checks.push(DiagnosticCheck {
            id: "sessions".to_owned(),
            status: CheckStatus::Ok,
            message: format!("{} session(s), {active} active", sessions.len()),
            remediation: None,
        });
        DiagnosticReport::new(checks, now())
    }

    fn metrics(&self) -> RuntimeMetrics {
        let sessions = self.manager.list();
        RuntimeMetrics {
            sessions: sessions.len() as u32,
            active_streams: sessions
                .iter()
                .filter(|session| {
                    matches!(
                        session.state,
                        SessionState::Streaming | SessionState::Degraded
                    )
                })
                .count() as u32,
            ..RuntimeMetrics::default()
        }
    }

    fn list_sessions(&self) -> Vec<Session> {
        self.manager.list()
    }

    fn get_session(&self, id: &SessionId) -> Option<Session> {
        self.manager.get(id).ok()
    }

    fn create_session(&self, spec: SessionSpec) -> Result<Session, ApiError> {
        // The HTTP layer will narrow this to a controller-issued grant in the
        // authentication pass. The local API currently runs on a trusted
        // transport (loopback or the authenticated node API).
        let granted = PermissionSet::all();
        self.manager.create(spec, granted, now())
    }

    fn start_session(&self, id: &SessionId) -> Result<Session, ApiError> {
        let session = self.manager.start(id)?;
        // The fake/local pipeline starts immediately. A transport-backed
        // session calls `mark_streaming` when the first media path is ready.
        match self.manager.mark_streaming(id) {
            Ok(session) => Ok(session),
            Err(_) => Ok(session),
        }
    }

    fn stop_session(&self, id: &SessionId, reason: EndReason) -> Result<Session, ApiError> {
        self.manager.stop(id, reason)
    }

    fn approve_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        self.manager.approve_consent(id, by)
    }

    fn deny_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        self.manager.deny_consent(id, by)
    }

    fn resume_session(
        &self,
        id: &SessionId,
        client_id: &ozma_protocol::ClientId,
    ) -> Result<Session, ApiError> {
        self.manager.resume(id, client_id)
    }

    fn transport_info(&self) -> Option<TransportInfo> {
        self.transport
            .lock()
            .expect("transport lock")
            .as_ref()
            .map(|transport| TransportInfo {
                endpoint: transport.endpoint.clone(),
                fingerprint: transport.certificate_fingerprint.clone(),
                certificate_der_base64: transport.certificate_der_base64.clone(),
            })
    }

    fn session_offer(&self, session: &Session) -> Result<SessionOffer, ApiError> {
        let transport = self
            .transport
            .lock()
            .expect("transport lock")
            .clone()
            .ok_or_else(|| {
                ApiError::new(
                    ozma_protocol::error::codes::TRANSPORT_UNAVAILABLE,
                    "No native transport endpoint is configured",
                )
            })?;

        let ttl = session.spec.ttl_seconds;
        let (token, _claims) = self.token_issuer.issue_new(
            session.id.clone(),
            session.spec.client_id.clone(),
            session.permissions.clone(),
            ttl,
            now(),
        )?;

        let descriptor = TransportDescriptorBuilder::quic(transport.endpoint)
            .certificate_fingerprint(transport.certificate_fingerprint.clone())
            .build();
        let mut offer = SessionOfferBuilder::new(session.id.clone())
            .token(token)
            .expires_in(now().saturating_mul(1_000), ttl)
            .transport(descriptor);

        // A browser cannot speak the raw QUIC listener, so advertise the
        // WebTransport endpoint as a second QUIC-family path. The raw
        // `quic://` descriptor stays first so native clients are unaffected.
        if let Some(webtransport) = self.webtransport.lock().expect("webtransport lock").clone() {
            let mut descriptor = TransportDescriptorBuilder::new(TransportMode::Quic)
                .endpoint(webtransport.endpoint)
                .secure(true)
                .alpn("h3")
                .channels(ChannelMap::default_session());
            if webtransport.certificate_fingerprint != transport.certificate_fingerprint {
                descriptor =
                    descriptor.certificate_fingerprint(webtransport.certificate_fingerprint);
            }
            offer = offer.transport(descriptor.build());
        }

        let offer = offer.build()?;
        if !offer
            .transports
            .iter()
            .any(|transport| transport.mode == TransportMode::Quic)
        {
            return Err(ApiError::internal("session offer has no QUIC transport"));
        }
        Ok(offer)
    }

    fn list_displays(&self) -> Vec<VirtualDisplaySpec> {
        self.displays
            .displays()
            .into_iter()
            .map(|display| display.spec)
            .collect()
    }

    fn create_display(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplaySpec, ApiError> {
        let display = self.displays.create(spec)?;
        Ok(display.spec)
    }

    fn destroy_display(&self, id: &DisplayId) -> Result<(), ApiError> {
        self.displays.destroy(id.clone())
    }

    fn topology(&self) -> DisplayTopology {
        self.displays.topology().unwrap_or_default()
    }

    fn set_display_mode(&self, id: &DisplayId, mode: DisplayMode) -> Result<(), ApiError> {
        self.displays.set_mode(id.clone(), mode)
    }

    fn set_display_dpms(&self, id: &DisplayId, state: DpmsState) -> Result<(), ApiError> {
        self.displays.set_dpms(id.clone(), state)
    }

    fn set_topology(&self, topology: DisplayTopology) -> Result<(), ApiError> {
        self.displays.set_topology(topology)
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_auth::MachineClass;
    use ozma_protocol::{ClientId, ConsentRequirement, InputSpec, PointerMode, PrivacyPolicy};

    fn backend() -> SessionBackend {
        SessionBackend::new(
            NodeCapabilities::fake("node"),
            ConsentPolicy::for_machine(MachineClass::Server),
        )
    }

    fn spec() -> SessionSpec {
        SessionSpec {
            client_id: ClientId::new("client"),
            principal: "alice".into(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: InputSpec {
                keyboard: true,
                pointer: PointerMode::Absolute,
                ..Default::default()
            },
            privacy: PrivacyPolicy {
                consent: ConsentRequirement::Never,
                ..Default::default()
            },
            transport: Default::default(),
            ttl_seconds: 60,
        }
    }

    #[test]
    fn create_start_stop_roundtrip() {
        let backend = backend();
        let session = backend.create_session(spec()).unwrap();
        let session = backend.start_session(&session.id).unwrap();
        assert_eq!(session.state, SessionState::Streaming);
        let stopped = backend
            .stop_session(&session.id, EndReason::AdminTerminated)
            .unwrap();
        assert_eq!(stopped.state, SessionState::Ended);
    }

    #[test]
    fn diagnostics_are_structured() {
        let report = backend().diagnostics();
        assert!(report.ok);
        assert!(report.checks.iter().any(|check| check.id == "encoder"));
    }

    #[test]
    fn consent_blocks_start_until_approved() {
        let backend = SessionBackend::new(
            NodeCapabilities::fake("node"),
            ConsentPolicy::new(MachineClass::Workstation, true),
        );
        let mut spec = spec();
        spec.privacy.consent = ConsentRequirement::Workstation;
        let session = backend.create_session(spec).unwrap();
        assert_eq!(session.state, SessionState::PendingConsent);

        let error = backend.start_session(&session.id).unwrap_err();
        assert_eq!(error.code, ozma_protocol::error::codes::SESSION_NOT_READY);

        let session = backend.approve_consent(&session.id, "local").unwrap();
        assert_eq!(session.state, SessionState::Preparing);
        let session = backend.start_session(&session.id).unwrap();
        assert_eq!(session.state, SessionState::Streaming);
    }
}
