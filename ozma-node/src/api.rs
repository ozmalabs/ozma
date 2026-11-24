// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Node services: the versioned HTTP API and the native QUIC session listener.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, Method, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use ozma_api::state::BrowserSceneSink;
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_encode::EncoderRegistry;
use ozma_input::{HidGadgetSink, SharedSink};
use ozma_protocol::NodeCapabilities;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::backend::{SessionBackend, WebTransportInfo};
use crate::browser_scene::BrowserSceneHub;
use crate::transport::QuicRuntime;
#[cfg(feature = "webtransport")]
use crate::transport::WebTransportRuntime;

/// Attach the host's detected encoders only when a real capture source exists.
///
/// `SessionBackend::with_encoder_registry` replaces the advertised `encode`
/// list. Attaching it unconditionally would make a node whose portal handshake
/// was denied (or whose X display/VM target is absent) look like it can encode
/// video. No source means no encode claim.
async fn backend_with_encoders(backend: SessionBackend) -> SessionBackend {
    if backend.capabilities().capture.is_empty() {
        backend.with_encoder_registry(EncoderRegistry::new())
    } else {
        let encoders = EncoderRegistry::with_detected_encoders().await;
        backend.with_encoder_registry(encoders)
    }
}

/// Run the node API and QUIC listener until either fails.
pub async fn serve(
    port: u16,
    quic_port: u16,
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
    api_token: Option<String>,
    rtsp_port: u16,
) -> Result<()> {
    serve_with_scene_replay(
        port,
        quic_port,
        capabilities,
        consent_policy,
        None,
        api_token,
        rtsp_port,
        None,
    )
    .await
}

/// Run the node API, the native QUIC listener, and (when `webtransport` is
/// `Some`) the browser WebTransport listener until the HTTP server exits.
pub async fn serve_with_scene_replay(
    port: u16,
    quic_port: u16,
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
    scene_replay: Option<crate::scene_replay::JsonlSceneProducer>,
    api_token: Option<String>,
    rtsp_port: u16,
    webtransport: Option<SocketAddr>,
) -> Result<()> {
    serve_with_scene_replay_and_sink(
        port,
        quic_port,
        capabilities,
        consent_policy,
        scene_replay,
        api_token,
        rtsp_port,
        webtransport,
        None,
        None,
        None,
        None,
        None,
        None,
        CancellationToken::new(),
    )
    .await
}

/// Run the node API with a shared input sink and an optional compatibility
/// router.
///
/// `input_sink` is the one sink used by both session input and the controller's
/// UDP HID listener. When it is `Some`, no second [`HidGadgetSink`] is opened
/// here. `compat_router` is merged into the HTTP API before the bearer-token
/// guard, so control endpoints such as `/api/v1/connection`, `/input/*`,
/// `/power/*`, `/audio/{apply,remove,play,stop}`, `/codec`, and `/webrtc/*`
/// are authenticated like the rest of the node API. `public_compat_router` is
/// merged **after** the guard: it carries only the controller's read-only
/// probes (`/display/snapshot`, `/display/info`, `/ui/hints`, `/node/status`,
/// `/agent/metrics`, `/audio/nodes`) and the always-unsupported
/// `POST /audio/sweep` stub, none of which change state or expose credentials.
/// `capture_registry` replaces the default V4L2/PipeWire registry; the VM
/// profile passes a QMP screendump registry.
///
/// `hls_dir` is mounted at `/stream` **outside** the bearer guard (the routes
/// are merged after the guard layer is applied). The Python controller proxies
/// `GET /stream/{file}` without credentials
/// (`controller/api.py::stream_hls_proxy`), so HLS media must be readable
/// unauthenticated; the guard still protects every control endpoint.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_scene_replay_and_sink(
    port: u16,
    quic_port: u16,
    capabilities: NodeCapabilities,
    consent_policy: ConsentPolicy,
    scene_replay: Option<crate::scene_replay::JsonlSceneProducer>,
    api_token: Option<String>,
    rtsp_port: u16,
    webtransport: Option<SocketAddr>,
    input_sink: Option<SharedSink>,
    compat_router: Option<Router>,
    public_compat_router: Option<Router>,
    capture_registry: Option<ozma_capture::CaptureRegistry>,
    hls_dir: Option<std::path::PathBuf>,
    multi_console_hls: Option<crate::multi_hls::MultiConsoleHls>,
    shutdown: CancellationToken,
) -> Result<()> {
    let backend = backend_with_encoders(SessionBackend::new(capabilities, consent_policy)).await;
    let backend = match capture_registry {
        Some(captures) => backend.with_capture_registry(captures),
        None => backend,
    };
    let backend = match input_sink {
        Some(sink) => backend.with_input_sink(sink),
        None => match HidGadgetSink::open() {
            Ok(sink) => backend.with_input_sink(Arc::new(sink)),
            Err(error) => {
                tracing::warn!(error = %error, "HID gadget unavailable; input will be recorded but not injected");
                backend
            }
        },
    };

    // The live browser hub is always the API ingest sink. Without a JSONL
    // replay it is also the session scene producer. The replay option keeps the
    // trusted fixture path available; in that mode live commits are still
    // acknowledged but the replay remains the session's scene source.
    let browser_scene = Arc::new(BrowserSceneHub::new());
    let backend = match scene_replay {
        Some(replay) => {
            warn!("scene replay configured; live browser commits will not replace the replay producer");
            backend.with_scene_producer(Arc::new(replay))
        }
        None => backend.with_scene_producer(browser_scene.clone()),
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
    let mut listeners: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

    let quic_backend = backend.clone();
    let quic_shutdown = shutdown.clone();
    listeners.push((
        "quic-listener",
        tokio::spawn(async move {
            tokio::select! {
                _ = quic_shutdown.cancelled() => {}
                result = crate::transport::run(quic, quic_backend) => {
                    if let Err(error) = result {
                        error!("Native QUIC listener exited: {error:#}");
                    }
                }
            }
        }),
    ));

    // Browser WebTransport listener. Its ECDSA P-256 identity is separate from
    // the raw QUIC certificate because browsers only accept certificates with a
    // validity of at most two weeks through `serverCertificateHashes`.
    match webtransport {
        Some(bind) => {
            #[cfg(feature = "webtransport")]
            {
                let runtime =
                    WebTransportRuntime::bind_on(bind).context("binding WebTransport service")?;
                backend.configure_webtransport_with_certificate(
                    runtime.endpoint(),
                    runtime.fingerprint(),
                    runtime.certificate_der(),
                );
                let webtransport_backend = backend.clone();
                let webtransport_shutdown = shutdown.clone();
                listeners.push((
                    "webtransport-listener",
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = webtransport_shutdown.cancelled() => {}
                            result = crate::transport::run_webtransport(
                                runtime,
                                webtransport_backend,
                            ) => {
                                if let Err(error) = result {
                                    error!("WebTransport listener exited: {error:#}");
                                }
                            }
                        }
                    }),
                ));
            }
            #[cfg(not(feature = "webtransport"))]
            {
                let _ = bind;
                error!(
                    "--webtransport-port was set but this ozma-node build has no \
                     `webtransport` feature; the browser listener is disabled"
                );
            }
        }
        None => {}
    }

    if rtsp_port != 0 {
        let rtsp_backend = backend.clone();
        let rtsp_shutdown = shutdown.clone();
        listeners.push((
            "rtsp-listener",
            tokio::spawn(async move {
                tokio::select! {
                    _ = rtsp_shutdown.cancelled() => {}
                    result = crate::rtsp_runtime::run(rtsp_port, rtsp_backend) => {
                        if let Err(error) = result {
                            error!("RTSP listener exited: {error:#}");
                        }
                    }
                }
            }),
        ));
    }

    let expiry_backend = backend.clone();
    let expiry_shutdown = shutdown.clone();
    listeners.push((
        "session-expiry",
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = expiry_shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let ended = expiry_backend.expire();
                        if ended > 0 {
                            info!(ended, "Expired idle sessions");
                        }
                    }
                }
            }
        }),
    ));

    let webtransport_info_backend = backend.clone();
    // The capture sink shares the same per-session hubs the QUIC/WebTransport
    // session loop decodes into.
    let capture_sink: Arc<dyn ozma_api::state::ClientCaptureSink> = backend.clone();
    let dynamic: Arc<dyn NodeBackend> = backend;
    let scene_sink: Arc<dyn BrowserSceneSink> = browser_scene;
    let app = match compat_router {
        Some(extra) => ozma_api::router::router_with_sinks_and_extra(
            dynamic,
            Some(scene_sink),
            Some(capture_sink),
            api_token.clone(),
            Some(extra),
        ),
        None => ozma_api::router::router_with_sinks(
            dynamic,
            Some(scene_sink),
            Some(capture_sink),
            api_token.clone(),
        ),
    };
    // Additive certificate endpoint for the browser client. It is public cert
    // material, but it is only mounted on the unauthenticated local/dev surface;
    // when a bearer token is configured, clients take the same values from the
    // session offer instead of bypassing the guard here.
    let app = if api_token.is_none() {
        app.merge(webtransport_info_router(webtransport_info_backend))
    } else {
        app
    };
    // Controller read-only probes are merged after the bearer layer on purpose:
    // the unmodified controller polls snapshot/info/hints/status/metrics and the
    // audio discovery/sweep stubs without credentials. This router contains no
    // control surface; see `controller_compat::public_router`.
    let app = match public_compat_router {
        Some(public) => app.merge(public),
        None => app,
    };
    // HLS media is merged after the bearer layer on purpose: the controller's
    // `/stream/*` proxy does not send credentials. See the doc comment on
    // `serve_with_scene_replay_and_sink`.
    let app = with_hls(app, hls_dir, multi_console_hls);
    let app = app.layer(dev_cors());
    info!(port, "Node API listening");
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await
        .context("serving node API");
    for (name, task) in listeners {
        crate::shutdown::stop_task(task, name).await;
    }
    served?;
    Ok(())
}

/// A deliberately permissive CORS layer for the local browser demo.
///
/// The wasm client is served from a separate dev origin (for example
/// `http://localhost:8080`) and calls the node API cross-origin. Any origin is
/// accepted **only** because this surface is loopback-oriented; tighten it to a
/// fixed origin allow-list before exposing the API to a LAN or the internet.
/// Credentials stay disabled so the bearer token must be sent explicitly, and
/// `preflight` `OPTIONS` is answered by this layer before the inner bearer
/// guard runs. The guard still protects the actual request.
fn dev_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
}

/// Merge the unauthenticated HLS file service when HLS is configured.
///
/// A per-console manager takes precedence and serves both
/// `/stream/console-<N>/...` and `/stream/...` for the active console. Without
/// it, the historical single-directory `ServeDir` is unchanged. `ServeDir`
/// rejects `..`, and the per-console router rejects separator/parent path
/// segments, so traversal cannot escape an output directory.
fn with_hls(
    app: Router,
    hls_dir: Option<std::path::PathBuf>,
    multi_console_hls: Option<crate::multi_hls::MultiConsoleHls>,
) -> Router {
    if let Some(manager) = multi_console_hls {
        // A single-head provider keeps the historical single-directory HLS
        // path; the manager is present only to report its typed limitation.
        if manager.supports_independent_consoles() {
            return app.merge(crate::multi_hls::stream_router(manager));
        }
    }
    match hls_dir {
        Some(directory) => app.merge(crate::hls::stream_router(directory)),
        None => app,
    }
}

#[derive(Clone)]
struct WebTransportInfoState {
    backend: Arc<SessionBackend>,
}

fn webtransport_info_router(backend: Arc<SessionBackend>) -> Router {
    Router::new()
        .route("/api/v1/webtransport", get(webtransport_info))
        .with_state(WebTransportInfoState { backend })
}

async fn webtransport_info(
    State(state): State<WebTransportInfoState>,
) -> Result<Json<WebTransportInfo>, StatusCode> {
    state
        .backend
        .webtransport_info()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
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
    use base64::Engine;
    use http_body_util::BodyExt;
    use ozma_protocol::scene::ImageResource;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration};
    use tower::ServiceExt;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nbrowser-ws";

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

    #[tokio::test]
    async fn no_capture_source_does_not_advertise_encoders() {
        let mut capabilities = NodeCapabilities::fake("no-capture");
        capabilities.capture.clear();
        capabilities.encode.clear();
        let backend = backend_with_encoders(SessionBackend::new(
            capabilities,
            ConsentPolicy::for_machine(MachineClass::Server),
        ))
        .await;
        assert!(backend.capabilities().capture.is_empty());
        assert!(
            backend.capabilities().encode.is_empty(),
            "detected encoders must not be advertised without a capture source"
        );
    }

    #[tokio::test]
    async fn hls_is_served_without_bearer_but_control_stays_guarded() {
        let directory = std::env::temp_dir().join(format!("ozma-api-hls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").unwrap();

        let backend: Arc<dyn NodeBackend> = Arc::new(SessionBackend::new(
            NodeCapabilities::fake("hls-guard"),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));
        let guarded = ozma_api::router::router_with_sinks_and_extra(
            backend,
            None,
            None,
            Some("secret".to_owned()),
            None,
        );
        let app = with_hls(guarded, Some(directory.clone()), None);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream/stream.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"#EXTM3U\n");

        let guarded_response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(guarded_response.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[derive(Debug)]
    struct RawWebSocket {
        stream: TcpStream,
    }

    impl RawWebSocket {
        async fn connect(
            address: std::net::SocketAddr,
            path: &str,
            token: Option<&str>,
        ) -> Result<Self, String> {
            let mut stream = TcpStream::connect(address)
                .await
                .map_err(|error| error.to_string())?;
            let key = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
            let mut request = format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: {address}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\n"
            );
            if let Some(token) = token {
                request.push_str(&format!("Authorization: Bearer {token}\r\n"));
            }
            request.push_str("\r\n");
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|error| error.to_string())?;

            let mut header = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream
                    .read_exact(&mut byte)
                    .await
                    .map_err(|error| error.to_string())?;
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
                if header.len() > 16 * 1024 {
                    return Err("WebSocket response headers are too large".to_owned());
                }
            }
            let head = String::from_utf8_lossy(&header);
            if !head.starts_with("HTTP/1.1 101") {
                return Err(head.lines().next().unwrap_or("handshake failed").to_owned());
            }
            Ok(Self { stream })
        }

        async fn send_text(&mut self, text: &str) -> Result<(), String> {
            let payload = text.as_bytes();
            let mut frame = Vec::new();
            frame.push(0x81);
            if payload.len() < 126 {
                frame.push(0x80 | payload.len() as u8);
            } else if payload.len() <= u16::MAX as usize {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            } else {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
            let mask = [0x12u8, 0x34, 0x56, 0x78];
            frame.extend_from_slice(&mask);
            for (index, byte) in payload.iter().enumerate() {
                frame.push(byte ^ mask[index % 4]);
            }
            self.stream
                .write_all(&frame)
                .await
                .map_err(|error| error.to_string())
        }

        async fn next_text(&mut self) -> Result<String, String> {
            loop {
                let mut header = [0u8; 2];
                self.stream
                    .read_exact(&mut header)
                    .await
                    .map_err(|error| error.to_string())?;
                let opcode = header[0] & 0x0f;
                let masked = header[1] & 0x80 != 0;
                let mut length = (header[1] & 0x7f) as u64;
                if length == 126 {
                    let mut bytes = [0u8; 2];
                    self.stream
                        .read_exact(&mut bytes)
                        .await
                        .map_err(|error| error.to_string())?;
                    length = u16::from_be_bytes(bytes) as u64;
                } else if length == 127 {
                    let mut bytes = [0u8; 8];
                    self.stream
                        .read_exact(&mut bytes)
                        .await
                        .map_err(|error| error.to_string())?;
                    length = u64::from_be_bytes(bytes);
                }
                let mut mask = [0u8; 4];
                if masked {
                    self.stream
                        .read_exact(&mut mask)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                let mut payload = vec![0u8; length as usize];
                self.stream
                    .read_exact(&mut payload)
                    .await
                    .map_err(|error| error.to_string())?;
                if masked {
                    for (index, byte) in payload.iter_mut().enumerate() {
                        *byte ^= mask[index % 4];
                    }
                }
                match opcode {
                    0x1 => return String::from_utf8(payload).map_err(|error| error.to_string()),
                    0x8 => return Err("socket closed".to_owned()),
                    _ => continue,
                }
            }
        }
    }

    async fn scene_router(
        hub: Arc<BrowserSceneHub>,
        token: Option<String>,
    ) -> std::net::SocketAddr {
        let backend: Arc<dyn NodeBackend> = Arc::new(SessionBackend::new(
            NodeCapabilities::fake("browser-ws"),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));
        let sink: Arc<dyn BrowserSceneSink> = hub;
        let app = ozma_api::router::router_with_token_and_scene(backend, Some(sink), token);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        address
    }

    #[tokio::test]
    async fn browser_scene_websocket_acks_resources_commits_and_video_metadata() {
        let hub = Arc::new(BrowserSceneHub::new());
        let address = scene_router(hub.clone(), None).await;
        timeout(Duration::from_secs(5), async {
            let mut socket = RawWebSocket::connect(address, "/api/v1/browser/scene/ws", None)
                .await
                .unwrap();

            // A malformed message produces an error but does not close.
            socket.send_text("{").await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&socket.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "error");
            assert_eq!(response["code"], "invalid_message");

            let framed = ImageResource { id: 7, png: PNG }.encode();
            let resource = serde_json::json!({
                "type": "resource",
                "id": 7,
                "payload_base64": base64::engine::general_purpose::STANDARD.encode(framed),
            });
            socket.send_text(&resource.to_string()).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&socket.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "ack");
            assert_eq!(response["kind"], "resource");
            assert_eq!(response["id"], 7);
            assert_eq!(hub.resource_count(), 1);

            let commit = serde_json::json!({
                "type": "scene_commit",
                "commit": {
                    "revision": 1,
                    "images": [{
                        "node_id": 1,
                        "resource_id": 7,
                        "x": 4,
                        "y": 8,
                        "width": 320,
                        "height": 180
                    }]
                }
            });
            socket.send_text(&commit.to_string()).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&socket.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "ack");
            assert_eq!(response["kind"], "scene_commit");
            assert_eq!(response["revision"], 1);
            assert_eq!(hub.last_revision(), 1);

            // A stale revision is an error, not a disconnect.
            socket.send_text(&commit.to_string()).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&socket.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "error");
            assert_eq!(response["code"], "stale_revision");

            let video = serde_json::json!({
                "type": "video_frame",
                "surface_id": 1,
                "frame": 12,
            });
            socket.send_text(&video.to_string()).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&socket.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "ack");
            assert_eq!(response["kind"], "video_frame");
            assert_eq!(response["surface_id"], 1);
            assert_eq!(hub.latest_video_frame().unwrap().frame, 12);
        })
        .await
        .expect("browser scene websocket timed out");
    }

    #[tokio::test]
    async fn browser_scene_websocket_uses_the_existing_bearer_guard() {
        let hub = Arc::new(BrowserSceneHub::new());
        let address = scene_router(hub, Some("secret-token".to_owned())).await;
        timeout(Duration::from_secs(5), async {
            let denied = RawWebSocket::connect(address, "/api/v1/browser/scene/ws", None)
                .await
                .unwrap_err();
            assert!(denied.contains("401"), "unexpected handshake: {denied}");

            let mut allowed =
                RawWebSocket::connect(address, "/api/v1/browser/scene/ws", Some("secret-token"))
                    .await
                    .unwrap();
            let video = serde_json::json!({
                "type": "video_frame",
                "surface_id": 3,
                "frame": 1,
            });
            allowed.send_text(&video.to_string()).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(&allowed.next_text().await.unwrap()).unwrap();
            assert_eq!(response["type"], "ack");
            assert_eq!(response["kind"], "video_frame");
        })
        .await
        .expect("browser scene bearer test timed out");
    }

    #[tokio::test]
    async fn webtransport_info_route_matches_the_configured_certificate() {
        let backend = Arc::new(SessionBackend::new(
            crate::platform::fake(),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));

        let empty = webtransport_info_router(backend.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/webtransport")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::NOT_FOUND);

        backend.configure_webtransport_with_certificate(
            "https://127.0.0.1:4443/session",
            "fingerprint-fixture",
            b"der-fixture",
        );
        let response = webtransport_info_router(backend)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/webtransport")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["endpoint"], "https://127.0.0.1:4443/session");
        assert_eq!(json["fingerprint"], "fingerprint-fixture");
        assert_eq!(
            json["certificate_der_base64"],
            base64::engine::general_purpose::STANDARD.encode(b"der-fixture")
        );
    }

    #[tokio::test]
    async fn cors_preflight_is_answered_before_the_bearer_guard() {
        let backend: Arc<dyn NodeBackend> = Arc::new(SessionBackend::new(
            crate::platform::fake(),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));
        let app = ozma_api::router::router_with_token_and_scene(
            backend,
            None,
            Some("secret-token".to_owned()),
        )
        .layer(dev_cors());

        // The browser preflight must succeed without the bearer token; the
        // layer answers it before the inner guard runs.
        let preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/v1/sessions")
                    .header(header::ORIGIN, "http://localhost:8080")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "authorization,content-type",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::OK);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        assert!(preflight
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .is_some());
        assert!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none(),
            "credentials must stay disabled"
        );

        // A real cross-origin request still needs the bearer token.
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sessions")
                    .header(header::ORIGIN, "http://localhost:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sessions")
                    .header(header::ORIGIN, "http://localhost:8080")
                    .header(header::AUTHORIZATION, "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert_eq!(
            allowed
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn public_controller_routes_bypass_bearer_but_controls_stay_guarded() {
        use crate::controller_compat::{
            compat_router, public_router, CompatStats, ConnectionInfo, ConnectionState,
            SINK_FAKE_RECORDING,
        };
        use crate::display_compat::SnapshotControl;
        use ozma_capture::{CaptureRegistry, FakeCaptureProvider};

        let backend: Arc<dyn NodeBackend> = Arc::new(SessionBackend::new(
            NodeCapabilities::fake("compat-guard"),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "compat-guard._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(FakeCaptureProvider::new()),
        )));
        let app = ozma_api::router::router_with_sinks_and_extra(
            backend,
            None,
            None,
            Some("secret-token".to_owned()),
            Some(compat_router(state.clone())),
        )
        .merge(public_router(state));

        // The controller pulls raw JPEG snapshots without credentials.
        let snapshot = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(snapshot.status(), StatusCode::OK);
        assert_eq!(snapshot.headers()[header::CONTENT_TYPE], "image/jpeg");
        let jpeg = snapshot.into_body().collect().await.unwrap().to_bytes();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "not a JPEG: {jpeg:?}");

        // The rest of the public read-only surface answers without a token,
        // and none of it echoes the configured secret.
        for uri in [
            "/display/info",
            "/ui/hints?level=2",
            "/node/status",
            "/agent/metrics",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/json",
                "{uri}"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let text = String::from_utf8(body.to_vec()).unwrap();
            assert!(!text.contains("secret-token"), "{uri} leaked the token");
            assert!(!text.contains("api_token"), "{uri} exposed a token field");
            assert!(
                !text.contains("Authorization"),
                "{uri} exposed auth headers"
            );
        }

        // Audio discovery/sweep are public typed 501 stubs, never 401 and never
        // a success-shaped no-op.
        for (method, uri, code) in [
            ("GET", "/audio/nodes", "audio-nodes-unsupported"),
            ("POST", "/audio/sweep", "audio-sweep-unsupported"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{uri}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["ok"], false, "{uri}");
            assert_eq!(json["available"], false, "{uri}");
            assert_eq!(json["code"], code, "{uri}");
        }

        // Control endpoints remain behind the bearer guard.
        for (method, uri) in [
            ("GET", "/power/state"),
            ("GET", "/codec"),
            ("GET", "/display/mjpeg"),
            ("GET", "/usb"),
            ("POST", "/usb/media/attach"),
            ("POST", "/usb/media/detach"),
            ("POST", "/input/key"),
            ("POST", "/audio/apply"),
            ("POST", "/webrtc/offer"),
        ] {
            let denied = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        // The same guarded route still works with the token.
        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/codec")
                    .header(header::AUTHORIZATION, "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }
}
