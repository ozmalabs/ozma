// SPDX-License-Identifier: AGPL-3.0-only
//! Axum routes for the native node API. Adapters (Hestia, Moonlight,
//! controller compatibility) can reuse the same backend.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{header, HeaderMap};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ozma_protocol::scene::SceneCommit;
use ozma_protocol::{
    ApiError, DiagnosticReport, DisplayId, DisplayMode, DisplayTopology, DpmsState, EndReason,
    NodeCapabilities, NodeId, RuntimeMetrics, Session, SessionEvent, SessionId, SessionOffer,
    SessionSpec, VirtualDisplaySpec,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiFailure;
use crate::state::{
    BrowserSceneSink, CaptureReport, ClientCaptureSink, NodeBackend, SceneSinkError,
};

type Backend = Arc<dyn NodeBackend>;
type ApiResult<T> = Result<Json<T>, ApiFailure>;

/// Largest browser scene message accepted before the socket is closed. Resource
/// payloads also have a protocol-level resource limit enforced by the sink.
const MAX_BROWSER_SCENE_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Give malformed messages a few chances to be corrected before treating a
/// stream as protocol abuse.
const MAX_BROWSER_SCENE_ERRORS: usize = 32;

#[derive(Clone)]
struct BrowserSceneState {
    sink: Option<Arc<dyn BrowserSceneSink>>,
}

#[derive(Clone)]
struct CaptureState {
    backend: Backend,
    sink: Option<Arc<dyn ClientCaptureSink>>,
}

/// Build the native API router with an optional static bearer token.
///
/// Health, capabilities, and Hestia capability discovery stay readable so a
/// controller or client can identify the node before it has credentials.
pub fn router_with_token(backend: Backend, token: Option<String>) -> Router {
    router_with_token_and_scene(backend, None, token)
}

/// Build the native API router with an optional live browser scene sink.
pub fn router_with_scene(backend: Backend, sink: Option<Arc<dyn BrowserSceneSink>>) -> Router {
    router_with_sinks(backend, sink, None, None)
}

/// Build the native API router with optional browser scene and capture sinks
/// plus an optional static bearer token.
pub fn router_with_sinks(
    backend: Backend,
    scene_sink: Option<Arc<dyn BrowserSceneSink>>,
    capture_sink: Option<Arc<dyn ClientCaptureSink>>,
    token: Option<String>,
) -> Router {
    let scene = Router::new()
        .route("/api/v1/browser/scene/ws", get(browser_scene_ws))
        .with_state(BrowserSceneState { sink: scene_sink });

    let capture = Router::new()
        .route("/api/v1/sessions/:id/capture", get(capture_report))
        .route(
            "/api/v1/sessions/:id/capture/frame.ppm",
            get(capture_frame_ppm),
        )
        .with_state(CaptureState {
            backend: backend.clone(),
            sink: capture_sink,
        });

    let api = Router::new()
        .route("/api/v1/health", get(health))
        .route("/health", get(health))
        .route("/api/v1/node", get(node))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/hestia/v1", get(hestia_capabilities))
        .route("/api/hestia/v1/capabilities", get(hestia_capabilities))
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/diagnostics/report", get(diagnostics_report))
        .route("/api/v1/metrics", get(metrics_json))
        .route("/api/v1/transport", get(transport_info))
        .route("/metrics", get(metrics_text))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/:id",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/sessions/:id/start", post(start_session))
        .route("/api/v1/sessions/:id/stop", post(stop_session))
        .route("/api/v1/sessions/:id/approve", post(approve_session))
        .route("/api/v1/sessions/:id/deny", post(deny_session))
        .route("/api/v1/sessions/:id/resume", post(resume_session))
        .route("/api/v1/sessions/:id/events", get(session_events))
        .route("/api/v1/displays", get(list_displays).post(create_display))
        .route("/api/v1/displays/:id", delete(delete_display))
        .route("/api/v1/displays/:id/mode", post(set_display_mode))
        .route("/api/v1/displays/:id/dpms", post(set_display_dpms))
        .route(
            "/api/v1/displays/topology",
            get(display_topology).put(set_display_topology),
        )
        .with_state(backend);

    let router = api.merge(scene).merge(capture);
    match token {
        Some(token) if !token.is_empty() => router.layer(middleware::from_fn_with_state(
            std::sync::Arc::new(token),
            require_bearer,
        )),
        _ => router,
    }
}

/// Build the native API router with a live browser scene sink and optional
/// static bearer token. The token guard also covers the browser scene socket
/// and the capture endpoints.
pub fn router_with_token_and_scene(
    backend: Backend,
    sink: Option<Arc<dyn BrowserSceneSink>>,
    token: Option<String>,
) -> Router {
    router_with_sinks(backend, sink, None, token)
}

async fn require_bearer(
    State(token): State<std::sync::Arc<String>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let path = request.uri().path();
    if path == "/health"
        || path == "/api/v1/health"
        || path == "/api/v1/capabilities"
        || path == "/api/hestia/v1"
        || path == "/api/hestia/v1/capabilities"
    {
        return Ok(next.run(request).await);
    }
    if bearer_matches(request.headers(), &token) {
        return Ok(next.run(request).await);
    }
    Err(ApiFailure(
        ApiError::new(
            ozma_protocol::error::codes::UNAUTHORIZED,
            "A bearer token is required",
        )
        .with_remediation(
            "Pass Authorization: Bearer <token> from the controller or local configuration.",
        ),
    ))
}

fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Build the native API router.
pub fn router(backend: Backend) -> Router {
    router_with_scene(backend, None)
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

/// A small Hestia-compatible capability document. It is derived from the same
/// node capabilities as the native API, so an existing Hestia client can
/// discover the host without a second configuration surface.
async fn hestia_capabilities(State(backend): State<Backend>) -> Json<serde_json::Value> {
    let caps = backend.capabilities();
    let codecs: Vec<&str> = caps
        .encode
        .iter()
        .map(|encoder| encoder.codec.as_str())
        .collect();
    Json(serde_json::json!({
        "ok": true,
        "server_name": caps.name,
        "base": "ozma",
        "hestia_protocol": 1,
        "min_client_protocol": 1,
        "max_client_protocol": 1,
        "server_version": backend.version(),
        "compatibility": {
            "gamestream": false,
            "moonlight": false,
            "sunshine": false,
        },
        "features": {
            "virtual_display": caps.display.supported,
            "remote_display": caps.display.supported,
            "clipboard_sync": caps.input.clipboard,
            "permission_system": true,
            "native_quic": caps.transport.contains(&ozma_protocol::TransportMode::Quic),
            "multi_session": caps.budget.max_sessions > 1,
            "multi_track": caps.budget.max_tracks > 1,
        },
        "limits": {
            "max_width": caps.capture.iter().map(|capture| capture.max_width).max().unwrap_or(0),
            "max_height": caps.capture.iter().map(|capture| capture.max_height).max().unwrap_or(0),
            "max_fps": caps.capture.iter().map(|capture| capture.max_fps).max().unwrap_or(0),
            "supported_codecs": codecs,
        },
    }))
}

async fn diagnostics(State(backend): State<Backend>) -> Json<DiagnosticReport> {
    Json(backend.diagnostics())
}

#[derive(Serialize)]
struct DiagnosticsReportResponse {
    report: DiagnosticReport,
    capabilities: NodeCapabilities,
    transport: Option<ozma_protocol::transport::TransportInfo>,
    version: &'static str,
}

async fn diagnostics_report(State(backend): State<Backend>) -> Json<DiagnosticsReportResponse> {
    Json(DiagnosticsReportResponse {
        report: backend.diagnostics(),
        capabilities: backend.capabilities(),
        transport: backend.transport_info(),
        version: backend.version(),
    })
}

async fn metrics_json(State(backend): State<Backend>) -> Json<RuntimeMetrics> {
    Json(backend.metrics())
}

async fn transport_info(
    State(backend): State<Backend>,
) -> Result<Json<ozma_protocol::transport::TransportInfo>, ApiFailure> {
    backend.transport_info().map(Json).ok_or_else(|| {
        ApiFailure(ApiError::new(
            ozma_protocol::error::codes::TRANSPORT_UNAVAILABLE,
            "This node has no native transport configured",
        ))
    })
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
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
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
    let session = backend.get_session(&session_id(id)).ok_or_else(|| {
        ApiError::new(ozma_protocol::error::codes::NOT_FOUND, "Session not found")
    })?;
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

#[derive(Debug, Deserialize)]
struct ResumeRequest {
    client_id: ozma_protocol::ClientId,
}

async fn resume_session(
    State(backend): State<Backend>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult<SessionResponse> {
    let session = backend.resume_session(&session_id(id), &request.client_id)?;
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

fn capture_unavailable() -> ApiError {
    ApiError::new(
        ozma_protocol::error::codes::NOT_FOUND,
        "Browser capture is not available for this session",
    )
    .with_remediation("Start a browser camera or microphone uplink for the session first.")
}

async fn capture_report(
    State(state): State<CaptureState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<CaptureReport> {
    let session_id = session_id(id);
    if state.backend.get_session(&session_id).is_none() {
        return Err(ApiFailure(ApiError::new(
            ozma_protocol::error::codes::NOT_FOUND,
            "Session not found",
        )));
    }
    let sink = state
        .sink
        .ok_or_else(|| ApiFailure(capture_unavailable()))?;
    let report = sink
        .capture_report(&session_id)
        .ok_or_else(|| ApiFailure(capture_unavailable()))?;
    Ok(Json(report))
}

async fn capture_frame_ppm(
    State(state): State<CaptureState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiFailure> {
    let session_id = session_id(id);
    if state.backend.get_session(&session_id).is_none() {
        return Err(ApiFailure(ApiError::new(
            ozma_protocol::error::codes::NOT_FOUND,
            "Session not found",
        )));
    }
    let sink = state
        .sink
        .ok_or_else(|| ApiFailure(capture_unavailable()))?;
    let frame = sink
        .capture_frame_ppm(&session_id)
        .ok_or_else(|| ApiFailure(capture_unavailable()))?;
    Ok(([(header::CONTENT_TYPE, "image/x-portable-pixmap")], frame).into_response())
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BrowserSceneMessage {
    Resource {
        id: u64,
        payload_base64: String,
    },
    SceneCommit {
        commit: SceneCommit,
    },
    VideoFrame {
        surface_id: u32,
        frame: serde_json::Value,
    },
}

/// Accept the browser's live resource, commit, and video-metadata stream.
/// Invalid individual messages produce an error reply and keep the socket
/// open; binary frames, oversized messages, and runaway malformed streams are
/// treated as protocol abuse and close the connection.
async fn browser_scene_ws(
    State(state): State<BrowserSceneState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.max_message_size(MAX_BROWSER_SCENE_MESSAGE_BYTES.saturating_mul(2))
        .on_upgrade(move |socket| browser_scene_socket(socket, state.sink))
}

async fn browser_scene_socket(mut socket: WebSocket, sink: Option<Arc<dyn BrowserSceneSink>>) {
    let Some(sink) = sink else {
        let _ = send_scene_json(
            &mut socket,
            serde_json::json!({
                "type": "error",
                "code": "unavailable",
                "message": "Browser scene ingest is not enabled on this node",
            }),
        )
        .await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let mut malformed = 0usize;
    loop {
        let incoming = match socket.recv().await {
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            Some(Ok(message)) => message,
        };
        match incoming {
            Message::Text(text) => {
                if text.len() > MAX_BROWSER_SCENE_MESSAGE_BYTES {
                    let _ = send_scene_json(
                        &mut socket,
                        scene_error_json(SceneSinkError::new(
                            "message_too_large",
                            format!(
                                "Message exceeds the {MAX_BROWSER_SCENE_MESSAGE_BYTES} byte limit"
                            ),
                        )),
                    )
                    .await;
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }

                let message: BrowserSceneMessage = match serde_json::from_str(&text) {
                    Ok(message) => {
                        malformed = 0;
                        message
                    }
                    Err(error) => {
                        malformed += 1;
                        if !send_scene_json(
                            &mut socket,
                            scene_error_json(SceneSinkError::new(
                                "invalid_message",
                                error.to_string(),
                            )),
                        )
                        .await
                        {
                            break;
                        }
                        if malformed >= MAX_BROWSER_SCENE_ERRORS {
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                        continue;
                    }
                };

                let response = match message {
                    BrowserSceneMessage::Resource { id, payload_base64 } => {
                        match sink.ingest_resource(id, &payload_base64) {
                            Ok(()) => serde_json::json!({
                                "type": "ack",
                                "kind": "resource",
                                "id": id,
                            }),
                            Err(error) => scene_error_json(error),
                        }
                    }
                    BrowserSceneMessage::SceneCommit { commit } => {
                        let revision = commit.revision;
                        let images = commit.images.len();
                        match sink.ingest_commit(commit) {
                            Ok(()) => serde_json::json!({
                                "type": "ack",
                                "kind": "scene_commit",
                                "revision": revision,
                                "images": images,
                            }),
                            Err(error) => scene_error_json(error),
                        }
                    }
                    BrowserSceneMessage::VideoFrame { surface_id, frame } => {
                        match sink.ingest_video_frame(surface_id, frame.clone()) {
                            Ok(()) => serde_json::json!({
                                "type": "ack",
                                "kind": "video_frame",
                                "surface_id": surface_id,
                                "frame": frame,
                            }),
                            Err(error) => scene_error_json(error),
                        }
                    }
                };
                if !send_scene_json(&mut socket, response).await {
                    break;
                }
            }
            Message::Binary(_) => {
                let _ = send_scene_json(
                    &mut socket,
                    scene_error_json(SceneSinkError::new(
                        "unsupported_binary",
                        "Browser scene messages must be JSON text frames",
                    )),
                )
                .await;
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }
}

fn scene_error_json(error: SceneSinkError) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "code": error.code,
        "message": error.message,
    })
}

async fn send_scene_json(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    socket.send(Message::Text(value.to_string())).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use ozma_protocol::{ClientId, InputSpec, PointerMode, SessionSpec, TrackKind, TrackSpec};
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
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
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
    async fn token_guard_requires_bearer_for_sessions() {
        let app = router_with_token(backend(), Some("secret-token".to_owned()));
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

        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/sessions")
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    struct FakeCaptureSink {
        report: CaptureReport,
        ppm: Option<Vec<u8>>,
    }

    impl ClientCaptureSink for FakeCaptureSink {
        fn capture_report(&self, _session_id: &ozma_protocol::SessionId) -> Option<CaptureReport> {
            Some(self.report.clone())
        }

        fn capture_frame_ppm(&self, _session_id: &ozma_protocol::SessionId) -> Option<Vec<u8>> {
            self.ppm.clone()
        }
    }

    fn capture_spec() -> SessionSpec {
        SessionSpec {
            client_id: ClientId::new("browser-1"),
            principal: "alice".into(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: Default::default(),
            privacy: Default::default(),
            transport: Default::default(),
            ttl_seconds: 60,
        }
    }

    fn capture_report_fixture() -> CaptureReport {
        CaptureReport {
            video: crate::state::CaptureVideoStats {
                started: true,
                packets: 12,
                frames: 10,
                bytes: 1_234,
                width: 1280,
                height: 720,
                codec: "h264".to_owned(),
                decode_errors: 0,
                last_sequence: 9,
            },
            audio: crate::state::CaptureAudioStats {
                started: true,
                packets: 50,
                bytes: 5_000,
                samples: 48_000,
                decode_errors: 0,
            },
        }
    }

    #[tokio::test]
    async fn capture_endpoints_report_json_and_ppm() {
        let backend: Backend = Arc::new(crate::state::MemoryBackend::fake());
        let session = backend.create_session(capture_spec()).unwrap();
        let ppm = b"P6\n2 2\n255\n\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b".to_vec();
        let sink = Arc::new(FakeCaptureSink {
            report: capture_report_fixture(),
            ppm: Some(ppm.clone()),
        });
        let app = router_with_sinks(backend, None, Some(sink), None);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/sessions/{}/capture", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["video"]["started"], true);
        assert_eq!(json["video"]["packets"], 12);
        assert_eq!(json["video"]["frames"], 10);
        assert_eq!(json["video"]["bytes"], 1_234);
        assert_eq!(json["video"]["width"], 1280);
        assert_eq!(json["video"]["height"], 720);
        assert_eq!(json["video"]["codec"], "h264");
        assert_eq!(json["video"]["decode_errors"], 0);
        assert_eq!(json["video"]["last_sequence"], 9);
        assert_eq!(json["audio"]["samples"], 48_000);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/sessions/{}/capture/frame.ppm", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), ppm.as_slice());
    }

    #[tokio::test]
    async fn capture_endpoints_are_404_until_a_frame_and_are_bearer_guarded() {
        let backend: Backend = Arc::new(crate::state::MemoryBackend::fake());
        let session = backend.create_session(capture_spec()).unwrap();
        let sink = Arc::new(FakeCaptureSink {
            report: CaptureReport::default(),
            ppm: None,
        });
        let app = router_with_sinks(backend, None, Some(sink), Some("secret-token".to_owned()));

        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/sessions/{}/capture", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let missing_frame = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/sessions/{}/capture/frame.ppm", session.id))
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_frame.status(), StatusCode::NOT_FOUND);
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
