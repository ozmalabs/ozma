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
use ozma_input::HidGadgetSink;
use ozma_protocol::NodeCapabilities;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::backend::{SessionBackend, WebTransportInfo};
use crate::browser_scene::BrowserSceneHub;
use crate::transport::QuicRuntime;
#[cfg(feature = "webtransport")]
use crate::transport::WebTransportRuntime;

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
    let encoders = EncoderRegistry::with_detected_encoders().await;
    let backend = SessionBackend::new(capabilities, consent_policy).with_encoder_registry(encoders);
    let backend = match HidGadgetSink::open() {
        Ok(sink) => backend.with_input_sink(Arc::new(sink)),
        Err(error) => {
            tracing::warn!(error = %error, "HID gadget unavailable; input will be recorded but not injected");
            backend
        }
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
    let quic_backend = backend.clone();
    tokio::spawn(async move {
        if let Err(error) = crate::transport::run(quic, quic_backend).await {
            error!("Native QUIC listener exited: {error:#}");
        }
    });

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
                tokio::spawn(async move {
                    if let Err(error) =
                        crate::transport::run_webtransport(runtime, webtransport_backend).await
                    {
                        error!("WebTransport listener exited: {error:#}");
                    }
                });
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
        tokio::spawn(async move {
            if let Err(error) = crate::rtsp_runtime::run(rtsp_port, rtsp_backend).await {
                error!("RTSP listener exited: {error:#}");
            }
        });
    }

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

    let webtransport_info_backend = backend.clone();
    // The capture sink shares the same per-session hubs the QUIC/WebTransport
    // session loop decodes into.
    let capture_sink: Arc<dyn ozma_api::state::ClientCaptureSink> = backend.clone();
    let dynamic: Arc<dyn NodeBackend> = backend;
    let scene_sink: Arc<dyn BrowserSceneSink> = browser_scene;
    let app = ozma_api::router::router_with_sinks(
        dynamic,
        Some(scene_sink),
        Some(capture_sink),
        api_token.clone(),
    );
    // Additive certificate endpoint for the browser client. It is public cert
    // material, but it is only mounted on the unauthenticated local/dev surface;
    // when a bearer token is configured, clients take the same values from the
    // session offer instead of bypassing the guard here.
    let app = if api_token.is_none() {
        app.merge(webtransport_info_router(webtransport_info_backend))
    } else {
        app
    };
    let app = app.layer(dev_cors());
    info!(port, "Node API listening");
    axum::serve(listener, app)
        .await
        .context("serving node API")?;
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
}
