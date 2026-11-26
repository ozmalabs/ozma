// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! AIDA64 RemoteSensor compatibility endpoint tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ozma_node::aida64;
use ozma_node::hardware::HardwareState;
use tower::ServiceExt;

fn app() -> axum::Router {
    aida64::router(HardwareState::new())
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn sensors_api_returns_aida_ids() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/sensors")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    let sensors = json["sensors"].as_array().unwrap();
    assert!(sensors.iter().all(|sensor| sensor["id"].is_string()));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn api_selection_returns_jsonp() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/api?callback=cb&sensors=TCPU,SCPUUTI")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_text(response).await;
    assert!(text.starts_with("cb("));
    assert!(text.ends_with(");"));
    let selected: serde_json::Value = serde_json::from_str(
        text.strip_prefix("cb(")
            .unwrap()
            .strip_suffix(");")
            .unwrap(),
    )
    .unwrap();
    assert!(selected
        .as_object()
        .unwrap()
        .keys()
        .all(|id| id == "TCPU" || id == "SCPUUTI"));
    assert!(!selected.as_object().unwrap().contains_key("SDATE"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn panel_layout_is_ozma_widget_json() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/panel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let layout: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(layout["id"], "aida64-panel");
    assert!(layout["widgets"].as_array().is_some());
    assert!(layout["fields"].as_array().is_some());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn panel_html_exposes_aida_simple_ids() {
    let response = app()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("id=\"Simple1\""));
    assert!(html.contains("EventSource('/sse')"));
}

#[tokio::test]
async fn invalid_jsonp_callback_is_rejected() {
    let response = app()
        .oneshot(
            Request::builder()
                .uri("/api?callback=bad;alert(1)")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn agent_metrics_includes_aida_sensor_ids() {
    use ozma_node::controller_compat::{
        public_router, CompatStats, ConnectionInfo, ConnectionState,
    };

    let state = ConnectionState::new(
        ConnectionInfo::new("test-node", "desktop", 7382, 7384, 7331, "uinput"),
        CompatStats::default(),
    )
    .with_hardware(HardwareState::new());
    let response = public_router(state)
        .oneshot(
            Request::builder()
                .uri("/agent/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    assert_eq!(json["aida64"], true);
    assert!(json["TCPU"].is_number() || json["SCPUUTI"].is_number());
}
