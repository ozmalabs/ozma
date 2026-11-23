// SPDX-License-Identifier: AGPL-3.0-only
//! Axum routes for the native node API. Adapters (Hestia, Moonlight,
//! controller compatibility) can reuse the same backend.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ozma_protocol::{
    ApiError, DiagnosticReport, DisplayId, DisplayMode, DisplayTopology, DpmsState, EndReason,
    NodeCapabilities, NodeId, RuntimeMetrics, Session, SessionEvent, SessionId, SessionOffer,
    SessionSpec, VirtualDisplaySpec,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiFailure;
use crate::state::NodeBackend;

type Backend = Arc<dyn NodeBackend>;
type ApiResult<T> = Result<Json<T>, ApiFailure>;

/// Build the native API router.
pub fn router(backend: Backend) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/node", get(node))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/metrics", get(metrics_json))
        .route("/metrics", get(metrics_text))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/:id", get(get_session).delete(delete_session))
        .route("/api/v1/sessions/:id/start", post(start_session))
        .route("/api/v1/sessions/:id/stop", post(stop_session))
        .route("/api/v1/sessions/:id/approve", post(approve_session))
        .route("/api/v1/sessions/:id/deny", post(deny_session))
        .route("/api/v1/sessions/:id/events", get(session_events))
        .route("/api/v1/displays", get(list_displays).post(create_display))
        .route("/api/v1/displays/:id", delete(delete_display))
        .route("/api/v1/displays/:id/mode", post(set_display_mode))
        .route("/api/v1/displays/:id/dpms", post(set_display_dpms))
        .route(
            "/api/v1/displays/topology",
            get(display_topology).put(set_display_topology),
        )
        .with_state(backend)
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    node_id: NodeId,
    version: &'static str,
    protocol: ozma_protocol::ProtocolVersion,
    sessions: usize,
}

async fn health(State(backend): State<Backend>) -> Json<HealthResponse> {
    let capabilities = backend.capabilities();
    Json(HealthResponse {
        ok: true,
        node_id: capabilities.node_id,
        version: backend.version(),
        protocol: capabilities.protocol,
        sessions: backend.list_sessions().len(),
    })
}

#[derive(Serialize)]
struct NodeResponse {
    node_id: NodeId,
    name: String,
    platform: String,
    version: &'static str,
    capabilities: NodeCapabilities,
}

async fn node(State(backend): State<Backend>) -> Json<NodeResponse> {
    let capabilities = backend.capabilities();
    Json(NodeResponse {
        node_id: capabilities.node_id.clone(),
        name: capabilities.name.clone(),
        platform: capabilities.platform.clone(),
        version: backend.version(),
        capabilities,
    })
}

async fn capabilities(State(backend): State<Backend>) -> Json<NodeCapabilities> {
    Json(backend.capabilities())
}

async fn diagnostics(State(backend): State<Backend>) -> Json<DiagnosticReport> {
    Json(backend.diagnostics())
}

async fn metrics_json(State(backend): State<Backend>) -> Json<RuntimeMetrics> {
    Json(backend.metrics())
}

async fn metrics_text(State(backend): State<Backend>) -> impl IntoResponse {
    let metrics = backend.metrics();
    let body = format!(
        "# HELP ozma_sessions Active sessions\n\
         # TYPE ozma_sessions gauge\n\
         ozma_sessions {}\n\
         # HELP ozma_streams Active media streams\n\
         # TYPE ozma_streams gauge\n\
         ozma_streams {}\n\
         # HELP ozma_fps Current encode frame rate\n\
         # TYPE ozma_fps gauge\n\
         ozma_fps {}\n\
         # HELP ozma_bitrate_kbps Current video bitrate\n\
         # TYPE ozma_bitrate_kbps gauge\n\
         ozma_bitrate_kbps {}\n\
         # HELP ozma_frames_dropped Dropped frame count\n\
         # TYPE ozma_frames_dropped counter\n\
         ozma_frames_dropped {}\n\
         # HELP ozma_rtt_ms Round-trip time in milliseconds\n\
         # TYPE ozma_rtt_ms gauge\n\
         ozma_rtt_ms {}\n\
         # HELP ozma_jitter_ms Jitter in milliseconds\n\
         # TYPE ozma_jitter_ms gauge\n\
         ozma_jitter_ms {}\n\
         # HELP ozma_loss_percent Packet loss percentage\n\
         # TYPE ozma_loss_percent gauge\n\
         ozma_loss_percent {}\n",
        metrics.sessions,
        metrics.active_streams,
        metrics.fps,
        metrics.bitrate_kbps,
        metrics.frames_dropped,
        metrics.rtt_ms,
        metrics.jitter_ms,
        metrics.loss_percent
    );
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

#[derive(Serialize)]
struct SessionsResponse {
    sessions: Vec<Session>,
}

async fn list_sessions(State(backend): State<Backend>) -> Json<SessionsResponse> {
    Json(SessionsResponse {
        sessions: backend.list_sessions(),
    })
}

#[derive(Serialize)]
struct SessionResponse {
    session: Session,
    #[serde(skip_serializing_if = "Option::is_none")]
    offer: Option<SessionOffer>,
}

fn session_response(backend: &Backend, session: Session) -> SessionResponse {
    let offer = backend.session_offer(&session).ok();
    SessionResponse { session, offer }
}

async fn create_session(
    State(backend): State<Backend>,
    Json(spec): Json<SessionSpec>,
) -> Result<(axum::http::StatusCode, Json<SessionResponse>), ApiFailure> {
    let session = backend.create_session(spec)?;
    let response = session_response(&backend, session);
    Ok((axum::http::StatusCode::CREATED, Json(response)))
}

fn session_id(value: String) -> SessionId {
    SessionId::new(value)
}

async fn get_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<SessionResponse> {
    let session = backend
        .get_session(&session_id(id))
        .ok_or_else(|| ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found"))?;
    Ok(Json(session_response(&backend, session)))
}

async fn start_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<SessionResponse> {
    let session = backend.start_session(&session_id(id))?;
    Ok(Json(session_response(&backend, session)))
}

#[derive(Debug, Default, Deserialize)]
struct StopRequest {
    #[serde(default)]
    reason: Option<EndReason>,
}

async fn stop_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<StopRequest>,
) -> ApiResult<SessionResponse> {
    let session = backend.stop_session(
        &session_id(id),
        request.reason.unwrap_or(EndReason::AdminTerminated),
    )?;
    Ok(Json(session_response(&backend, session)))
}

async fn delete_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<SessionResponse> {
    let session = backend.stop_session(&session_id(id), EndReason::AdminTerminated)?;
    Ok(Json(session_response(&backend, session)))
}

#[derive(Debug, Deserialize)]
struct DecisionRequest {
    #[serde(default)]
    by: Option<String>,
}

async fn approve_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<DecisionRequest>,
) -> ApiResult<SessionResponse> {
    let by = request.by.unwrap_or_else(|| "local".to_owned());
    let session = backend.approve_consent(&session_id(id), &by)?;
    Ok(Json(session_response(&backend, session)))
}

async fn deny_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<DecisionRequest>,
) -> ApiResult<SessionResponse> {
    let by = request.by.unwrap_or_else(|| "local".to_owned());
    let session = backend.deny_consent(&session_id(id), &by)?;
    Ok(Json(session_response(&backend, session)))
}

async fn session_events(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let id = session_id(id);
    ws.on_upgrade(move |socket| events_socket(socket, backend, id))
}

async fn events_socket(mut socket: WebSocket, backend: Backend, session_id: SessionId) {
    let mut events = backend.subscribe();
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if !event_belongs(&event, &session_id) {
                            continue;
                        }
                        let Ok(text) = serde_json::to_string(&event) else { continue };
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

fn event_belongs(event: &SessionEvent, id: &SessionId) -> bool {
    match event {
        SessionEvent::StateChanged { session_id, .. }
        | SessionEvent::ConsentDecided { session_id, .. }
        | SessionEvent::TrackStarted { session_id, .. }
        | SessionEvent::TrackStopped { session_id, .. }
        | SessionEvent::InputReleased { session_id, .. }
        | SessionEvent::DisplayChanged { session_id, .. }
        | SessionEvent::Error { session_id, .. }
        | SessionEvent::Ended { session_id, .. } => session_id == id,
        SessionEvent::ConsentRequested { request } => &request.session_id == id,
    }
}

#[derive(Serialize)]
struct DisplaysResponse {
    displays: Vec<VirtualDisplaySpec>,
}

async fn list_displays(State(backend): State<Backend>) -> Json<DisplaysResponse> {
    Json(DisplaysResponse {
        displays: backend.list_displays(),
    })
}

async fn create_display(
    State(backend): State<Backend>,
    Json(spec): Json<VirtualDisplaySpec>,
) -> Result<(axum::http::StatusCode, Json<VirtualDisplaySpec>), ApiFailure> {
    let spec = backend.create_display(spec)?;
    Ok((axum::http::StatusCode::CREATED, Json(spec)))
}

async fn delete_display(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
) -> Result<axum::http::StatusCode, ApiFailure> {
    backend.destroy_display(&DisplayId::new(id))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn display_topology(State(backend): State<Backend>) -> Json<DisplayTopology> {
    Json(backend.topology())
}

async fn set_display_mode(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(mode): Json<DisplayMode>,
) -> Result<axum::http::StatusCode, ApiFailure> {
    backend.set_display_mode(&DisplayId::new(id), mode)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn set_display_dpms(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(state): Json<DpmsState>,
) -> Result<axum::http::StatusCode, ApiFailure> {
    backend.set_display_dpms(&DisplayId::new(id), state)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn set_display_topology(
    State(backend): State<Backend>,
    Json(topology): Json<DisplayTopology>,
) -> Result<axum::http::StatusCode, ApiFailure> {
    backend.set_topology(topology)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use ozma_protocol::{ClientId, InputSpec, PointerMode, TrackKind, TrackSpec};
    use tower::ServiceExt;

    fn backend() -> Backend {
        Arc::new(crate::state::MemoryBackend::fake())
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_and_capabilities() {
        let app = router(backend());
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["protocol"]["major"], 1);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["display"]["supported"], true);
    }

    #[tokio::test]
    async fn creates_and_starts_a_session() {
        let app = router(backend());
        let spec = SessionSpec {
            client_id: ClientId::new("client-1"),
            principal: "alice".into(),
            tracks: vec![TrackSpec {
                track_id: ozma_protocol::TrackId::new("video0"),
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
        let response = app
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
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        let id = json["session"]["id"].as_str().unwrap().to_owned();

        let response = app
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
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["session"]["state"], "streaming");
    }

    #[tokio::test]
    async fn unknown_session_is_a_structured_404() {
        let app = router(backend());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sessions/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let json = body_json(response).await;
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "not_found");
    }
}
