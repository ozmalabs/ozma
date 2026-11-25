// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Hardware inventory API integration tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ozma_node::hardware::{router, HardwareState};
use tower::ServiceExt;

#[test]
fn hardware_router_builds() {
    let _ = router(HardwareState::new());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn hardware_route_returns_a_cpu_and_memory_tree() {
    let app = router(HardwareState::new());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/hardware")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!json["cpu"]["brand"].as_str().unwrap_or("").is_empty());
    assert!(json["total_memory_gb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(json["cpu"]["cores"].is_array());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn hardware_summary_route_is_smaller_but_keeps_sensor_sections() {
    let app = router(HardwareState::new());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/hardware/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["fans"].is_array());
    assert!(json["voltages"].is_array());
    assert!(json["gpus"].is_array());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn hardware_sensors_route_returns_the_raw_sensor_list() {
    let app = router(HardwareState::new());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/hardware/sensors")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.is_array());
}
