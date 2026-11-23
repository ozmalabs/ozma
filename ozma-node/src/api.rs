// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Node services: the versioned HTTP API and the native QUIC session listener.

use std::sync::Arc;

use anyhow::{Context, Result};
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_input::HidGadgetSink;
use ozma_protocol::NodeCapabilities;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::backend::SessionBackend;
use crate::transport::QuicRuntime;

/// Run the node API and QUIC listener until either fails.
pub async fn serve(
    port: u16,
    quic_port: u16,
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
) -> Result<()> {
    let backend = SessionBackend::new(capabilities, consent_policy);
    let backend = match HidGadgetSink::open() {
        Ok(sink) => backend.with_input_sink(Arc::new(sink)),
        Err(error) => {
            tracing::warn!(error = %error, "HID gadget unavailable; input will be recorded but not injected");
            backend
        }
    };
    let backend = Arc::new(backend);

    // Bind the HTTP listener first: a node that cannot serve its API must not
    // leave a QUIC listener behind on a half-started process.
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding node API on port {port}"))?;

    let quic = QuicRuntime::bind(quic_port).context("binding native QUIC service")?;
    backend.configure_transport_with_certificate(
        quic.endpoint(),
        quic.fingerprint(),
        quic.certificate_der(),
    );
    let quic_backend = backend.clone();
    tokio::spawn(async move {
        if let Err(error) = crate::transport::run(quic, quic_backend).await {
            error!("Native QUIC listener exited: {error:#}");
        }
    });

    let expiry_backend = backend.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let ended = expiry_backend.expire();
            if ended > 0 {
                info!(ended, "Expired idle sessions");
            }
        }
    });

    let dynamic: Arc<dyn NodeBackend> = backend;
    let app = ozma_api::router(dynamic);
    info!(port, "Node API listening");
    axum::serve(listener, app)
        .await
        .context("serving node API")?;
    Ok(())
}

/// Build the HTTP router without binding, for tests and embedding.
pub fn router(capabilities: NodeCapabilities) -> axum::Router {
    router_with_policy(
        capabilities,
        ConsentPolicy::for_machine(MachineClass::Server),
    )
}

/// Build the HTTP router with an explicit consent policy.
pub fn router_with_policy(
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
) -> axum::Router {
    let backend: Arc<dyn NodeBackend> = Arc::new(SessionBackend::new(capabilities, consent_policy));
    ozma_api::router(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_route_is_served() {
        let app = router(crate::platform::fake());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
    }
}
