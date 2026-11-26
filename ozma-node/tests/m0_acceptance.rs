// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! M0 acceptance: the node API serves capabilities and a fake session can
//! reach `Streaming` through the public routes, without hardware.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ozma_node::api;
use ozma_node::platform;
use ozma_protocol::{ClientId, InputSpec, PointerMode, SessionSpec, TrackId, TrackKind, TrackSpec};
use tower::ServiceExt;

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Session routes are loopback/same-origin guarded; the acceptance test drives
/// the router directly, so it supplies the loopback peer and browser origin a
/// real local UI would use.
fn local_request(method: &str, uri: String, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:3000")
        .header("origin", "http://127.0.0.1:3000")
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn m0_fake_session_reaches_streaming() {
    let app = api::router(platform::fake());

    let health = app
        .clone()
        .oneshot(local_request(
            "GET",
            "/api/v1/health".to_owned(),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let health = json(health).await;
    assert_eq!(health["ok"], true);

    let capabilities = app
        .clone()
        .oneshot(local_request(
            "GET",
            "/api/v1/capabilities".to_owned(),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(capabilities.status(), StatusCode::OK);
    let capabilities = json(capabilities).await;
    assert_eq!(capabilities["protocol"]["major"], 1);

    let spec = SessionSpec {
        client_id: ClientId::new("acceptance-client"),
        principal: "acceptance".into(),
        tracks: vec![TrackSpec {
            track_id: TrackId::new("video0"),
            kind: TrackKind::Capture,
            source_id: Some("fake0".into()),
            display: None,
            format: None,
            mode: None,
            hdr: false,
        }],
        audio: Default::default(),
        input: InputSpec {
            keyboard: true,
            pointer: PointerMode::Absolute,
            ..Default::default()
        },
        privacy: Default::default(),
        transport: Default::default(),
        quality: None,
        ttl_seconds: 60,
    };

    let created = app
        .clone()
        .oneshot(local_request(
            "POST",
            "/api/v1/sessions".to_owned(),
            Body::from(serde_json::to_vec(&spec).unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json(created).await;
    let id = created["session"]["id"].as_str().unwrap().to_owned();

    // Requesting control permissions forces local consent at the node, so the
    // acceptance flow must approve the pending request before it can start.
    let approved = app
        .clone()
        .oneshot(local_request(
            "POST",
            format!("/api/v1/sessions/{id}/approve"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(approved.status(), StatusCode::OK);

    let started = app
        .clone()
        .oneshot(local_request(
            "POST",
            format!("/api/v1/sessions/{id}/start"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let started = json(started).await;
    assert_eq!(started["session"]["state"], "streaming");

    let fetched = app
        .oneshot(local_request(
            "GET",
            format!("/api/v1/sessions/{id}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched = json(fetched).await;
    assert_eq!(fetched["session"]["state"], "streaming");
}
