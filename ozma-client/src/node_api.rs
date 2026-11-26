// SPDX-License-Identifier: AGPL-3.0-only
//! Client for the node's own HTTP API.
//!
//! The session control plane is `POST /api/v1/sessions` (create),
//! `POST /api/v1/sessions/{id}/start`, `POST /api/v1/sessions/{id}/approve`,
//! and `GET /api/v1/sessions/{id}`. Each returns the same `{session, offer}`
//! envelope, where the offer carries the short-lived session token and the
//! QUIC transport descriptor.
//!
//! `GET /api/v1/transport` returns the native QUIC endpoint, the SHA-256
//! certificate fingerprint, and the DER certificate as base64. Fetching it is
//! the headless equivalent of copying a certificate file: the DER is decoded
//! and checked against the advertised fingerprint before it is used to pin the
//! QUIC connection.

use std::time::Duration;

use ozma_protocol::transport::TransportInfo;
use ozma_protocol::{ApiError, Session, SessionId, SessionOffer, SessionSpec};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cert;
use crate::error;
use crate::http::{self, DEFAULT_HTTP_TIMEOUT};

/// The node transport description plus decoded DER certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedTransportInfo {
    /// The protocol-level transport info exactly as the node returned it.
    pub info: TransportInfo,
    /// Base64-decoded DER certificate, ready for QUIC pinning.
    #[serde(skip)]
    pub certificate_der: Vec<u8>,
}

impl FetchedTransportInfo {
    /// The advertised QUIC endpoint (`host:port`).
    pub fn endpoint(&self) -> &str {
        &self.info.endpoint
    }

    /// The advertised SHA-256 fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.info.fingerprint
    }
}

/// Session responses are `{session, offer?}` today. A bare session object is
/// accepted too so that older nodes and fixtures keep working.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum SessionPayload {
    Wrapped {
        session: Session,
        #[serde(default)]
        offer: Option<SessionOffer>,
    },
    Bare(Session),
}

impl SessionPayload {
    fn into_parts(self) -> (Session, Option<SessionOffer>) {
        match self {
            SessionPayload::Wrapped { session, offer } => (session, offer),
            SessionPayload::Bare(session) => (session, None),
        }
    }
}

/// Builder for [`NodeApiClient`].
#[derive(Clone, Debug)]
pub struct NodeApiClientBuilder {
    base_url: String,
    token: Option<String>,
    insecure: bool,
    timeout: Duration,
    no_proxy: bool,
}

impl NodeApiClientBuilder {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: None,
            insecure: false,
            timeout: DEFAULT_HTTP_TIMEOUT,
            no_proxy: false,
        }
    }

    /// Optional bearer token for a node API that requires auth.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Accept invalid TLS certificates (local bring-up only).
    pub fn insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn no_proxy(mut self, no_proxy: bool) -> Self {
        self.no_proxy = no_proxy;
        self
    }

    pub fn build(self) -> Result<NodeApiClient, ApiError> {
        let base_url = http::normalize_base_url(&self.base_url)?;
        let http = http::build_client(
            self.token.as_deref(),
            self.insecure,
            self.timeout,
            self.no_proxy,
        )?;
        Ok(NodeApiClient { base_url, http })
    }
}

/// Typed client for the node HTTP API.
#[derive(Clone, Debug)]
pub struct NodeApiClient {
    base_url: String,
    http: Client,
}

impl NodeApiClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, ApiError> {
        Self::builder(base_url).build()
    }

    pub fn builder(base_url: impl Into<String>) -> NodeApiClientBuilder {
        NodeApiClientBuilder::new(base_url)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Create a session with `POST /api/v1/sessions`.
    ///
    /// Returns the session record and, when the node has a native transport,
    /// the short-lived [`SessionOffer`] that is used to connect over QUIC.
    pub async fn create_session(
        &self,
        spec: &SessionSpec,
    ) -> Result<(Session, Option<SessionOffer>), ApiError> {
        let url = http::join_url(&self.base_url, "/api/v1/sessions")?;
        let request = self.http.request(Method::POST, url).json(spec);
        let payload: SessionPayload = http::send_json(request, "node session creation").await?;
        Ok(payload.into_parts())
    }

    /// Start a created or consented session with
    /// `POST /api/v1/sessions/{id}/start`.
    pub async fn start_session(&self, id: &SessionId) -> Result<Session, ApiError> {
        Ok(self.start_session_with_offer(id).await?.0)
    }

    /// Start a session and keep the returned transport offer.
    pub async fn start_session_with_offer(
        &self,
        id: &SessionId,
    ) -> Result<(Session, Option<SessionOffer>), ApiError> {
        let url = http::join_url(&self.base_url, &format!("{}/start", self.session_path(id)))?;
        let payload: SessionPayload =
            http::send_json(self.http.request(Method::POST, url), "node session start").await?;
        Ok(payload.into_parts())
    }

    /// Fetch one session with `GET /api/v1/sessions/{id}`.
    pub async fn get_session(&self, id: &SessionId) -> Result<Session, ApiError> {
        Ok(self.get_session_with_offer(id).await?.0)
    }

    /// Fetch one session and its current transport offer, if any.
    pub async fn get_session_with_offer(
        &self,
        id: &SessionId,
    ) -> Result<(Session, Option<SessionOffer>), ApiError> {
        let url = http::join_url(&self.base_url, &self.session_path(id))?;
        let payload: SessionPayload =
            http::send_json(self.http.request(Method::GET, url), "node session lookup").await?;
        Ok(payload.into_parts())
    }

    /// Approve a session waiting in `pending_consent` with
    /// `POST /api/v1/sessions/{id}/approve` and a `{"by": ...}` body.
    pub async fn approve_consent(&self, id: &SessionId, by: &str) -> Result<Session, ApiError> {
        let url = http::join_url(
            &self.base_url,
            &format!("{}/approve", self.session_path(id)),
        )?;
        let request = self
            .http
            .request(Method::POST, url)
            .json(&json!({ "by": by }));
        let payload: SessionPayload =
            http::send_json(request, "node session consent approval").await?;
        Ok(payload.into_parts().0)
    }

    /// Path for one session, with the id percent-encoded as a single segment.
    pub(crate) fn session_path(&self, id: &SessionId) -> String {
        format!("/api/v1/sessions/{}", urlencoding::encode(id.as_str()))
    }

    /// Fetch `/api/v1/transport` and decode + verify the certificate.
    pub async fn transport_info(&self) -> Result<FetchedTransportInfo, ApiError> {
        let url = http::join_url(&self.base_url, "/api/v1/transport")?;
        let info: TransportInfo =
            http::send_json(self.http.request(Method::GET, url), "node transport info").await?;

        let certificate_der = cert::certificate_der_from_base64(&info.certificate_der_base64)?;
        if !info.fingerprint.trim().is_empty() {
            cert::verify_certificate_fingerprint(&certificate_der, &info.fingerprint).map_err(
                |error| {
                    error.with_details(json!({
                        "endpoint": info.endpoint,
                        "fingerprint": info.fingerprint,
                        "actual_fingerprint": cert::certificate_fingerprint(&certificate_der),
                    }))
                },
            )?;
        }
        if info.endpoint.trim().is_empty() {
            return Err(error::config(
                "The node transport response has an empty endpoint",
            ));
        }

        Ok(FetchedTransportInfo {
            info,
            certificate_der,
        })
    }
}

/// Convenience function used by the CLI and examples.
pub async fn fetch_transport_info(
    node_url: impl Into<String>,
    token: Option<&str>,
    insecure: bool,
) -> Result<FetchedTransportInfo, ApiError> {
    let mut builder = NodeApiClient::builder(node_url).insecure(insecure);
    if let Some(token) = token {
        builder = builder.token(token);
    }
    builder.build()?.transport_info().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use ozma_transport::SelfSignedCertificate;

    #[test]
    fn session_path_encodes_the_id_as_one_segment() {
        let client = NodeApiClient::new("http://127.0.0.1:7390").unwrap();
        assert_eq!(
            client.session_path(&SessionId::new("sess a/b")),
            "/api/v1/sessions/sess%20a%2Fb"
        );
    }

    #[test]
    fn session_payload_accepts_wrapped_and_bare_sessions() {
        use ozma_protocol::{ClientId, SessionId, SessionSpec};

        let spec = SessionSpec {
            client_id: ClientId::new("client-test"),
            principal: "test".to_string(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: Default::default(),
            privacy: Default::default(),
            transport: Default::default(),
            quality: None,
            ttl_seconds: 60,
        };
        let session = Session::new(
            SessionId::new("sess-1"),
            spec,
            ozma_protocol::PermissionSet::all(),
            1,
        );

        let bare: SessionPayload =
            serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        let (parsed, offer) = bare.into_parts();
        assert_eq!(parsed.id.as_str(), "sess-1");
        assert!(offer.is_none());

        let wrapped: SessionPayload = serde_json::from_value(json!({
            "session": session,
            "offer": null,
        }))
        .unwrap();
        let (parsed, offer) = wrapped.into_parts();
        assert_eq!(parsed.id.as_str(), "sess-1");
        assert!(offer.is_none());
    }

    #[test]
    fn fetched_info_exposes_the_advertised_values() {
        let certificate = SelfSignedCertificate::generate(&["localhost".to_string()]).unwrap();
        let der = certificate.certificate_der().to_vec();
        let info = TransportInfo {
            endpoint: "127.0.0.1:4433".to_string(),
            fingerprint: cert::certificate_fingerprint(&der),
            certificate_der_base64: STANDARD.encode(&der),
        };
        let fetched = FetchedTransportInfo {
            info,
            certificate_der: der.clone(),
        };
        assert_eq!(fetched.endpoint(), "127.0.0.1:4433");
        assert_eq!(fetched.certificate_der, der);
        assert!(cert::verify_certificate_fingerprint(
            &fetched.certificate_der,
            fetched.fingerprint()
        )
        .is_ok());
    }
}
