// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! HTTP API server for the node contract.

use std::sync::Arc;

use anyhow::{Context, Result};
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_protocol::NodeCapabilities;
use tokio::net::TcpListener;
use tracing::info;

use crate::backend::SessionBackend;

/// Run the node API until the task is cancelled or the listener fails.
pub async fn serve(
    port: u16,
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
) -> Result<()> {
    let backend: Arc<dyn NodeBackend> =
        Arc::new(SessionBackend::new(capabilities, consent_policy));
    let app = ozma_api::router(backend);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding node API on port {port}"))?;
    info!(port, "Node API listening");
    axum::serve(listener, app).await.context("serving node API")?;
    Ok(())
}

/// Build the router without binding, for tests and embedding.
pub fn router(capabilities: NodeCapabilities) -> axum::Router {
    router_with_policy(
        capabilities,
        ConsentPolicy::for_machine(MachineClass::Server),
    )
}

/// Build the router with an explicit consent policy.
pub fn router_with_policy(
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
) -> axum::Router {
    let backend: Arc<dyn NodeBackend> =
        Arc::new(SessionBackend::new(capabilities, consent_policy));
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
            .oneshot(Request::builder().uri("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
    }
}
