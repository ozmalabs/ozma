// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! M0 acceptance: the node API serves capabilities and a fake session can
//! reach `Streaming` through the public routes, without hardware.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ozma_node::api;
use ozma_node::platform;
use ozma_protocol::{
    ClientId, InputSpec, PointerMode, SessionSpec, TrackId, TrackKind, TrackSpec,
};
use tower::ServiceExt;

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn m0_fake_session_reaches_streaming() {
    let app = api::router(platform::fake());

    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let health = json(health).await;
    assert_eq!(health["ok"], true);

    let capabilities = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
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
        ttl_seconds: 60,
    };

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&spec).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json(created).await;
    let id = created["session"]["id"].as_str().unwrap().to_owned();

    let started = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/sessions/{id}/start"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let started = json(started).await;
    assert_eq!(started["session"]["state"], "streaming");

    let fetched = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sessions/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched = json(fetched).await;
    assert_eq!(fetched["session"]["state"], "streaming");
}
