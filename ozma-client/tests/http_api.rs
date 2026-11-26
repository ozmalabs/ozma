// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests for the controller and node HTTP APIs.
//!
//! A tiny one-shot HTTP server records the real method, path, and auth header;
//! no controller or node process is required.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ozma_client::{certificate_fingerprint, codes, ControllerClient, NodeApiClient};
use ozma_protocol::{
    AudioSpec, ClientId, DisplayMode, InputSpec, Permission, PermissionSet, PointerMode,
    PrivacyPolicy, Session, SessionId, SessionSpec, SessionState, TrackId, TrackKind, TrackSpec,
    TransportPreference,
};
use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct Response {
    status: u16,
    body: String,
}

impl Response {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// Start a server that handles exactly one request per response, in order.
async fn mock_server(responses: Vec<Response>) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();

    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            sink.lock().await.push(request);
            write_response(&mut socket, response).await;
        }
    });

    (format!("http://{address}"), recorded)
}

async fn read_request(socket: &mut TcpStream) -> RecordedRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            break buffer.len();
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end.min(buffer.len())]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut authorization = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            }
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let expected = header_end.saturating_add(content_length).min(1024 * 1024);
    while buffer.len() < expected {
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[header_end.min(buffer.len())..]).to_string();

    RecordedRequest {
        method,
        path,
        authorization,
        body,
    }
}

async fn write_response(socket: &mut TcpStream, response: Response) {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let body = response.body;
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        body.as_bytes().len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.shutdown().await.ok();
}

#[tokio::test]
async fn controller_list_get_and_activate_use_the_real_api() {
    let (base_url, recorded) = mock_server(vec![
        Response::ok(
            json!({
                "nodes": [
                    {
                        "id": "node-a",
                        "host": "10.0.0.5",
                        "api_port": 7390,
                        "capabilities": ["hid", "video"],
                        "last_seen": 123.5,
                        "future": {"ignored": true}
                    },
                    {
                        "id": "node-b",
                        "name": "Desk node",
                        "host": "10.0.0.6",
                        "api_port": "7391",
                        "online": false,
                        "capabilities": []
                    }
                ],
                "active_node_id": "node-b",
                "unexpected": "ignored"
            })
            .to_string(),
        ),
        Response::ok(
            json!({
                "id": "node-a",
                "host": "10.0.0.5",
                "api_port": 7390,
                "capabilities": ["hid"],
                "hid_active": true
            })
            .to_string(),
        ),
        Response::ok(json!({"ok": true, "active_node_id": "node-a"}).to_string()),
    ])
    .await;

    let client = ControllerClient::builder(&base_url)
        .token("secret-token")
        .no_proxy(true)
        .build()
        .unwrap();

    let list = client.list_nodes_response().await.unwrap();
    assert_eq!(list.active_node_id.as_deref(), Some("node-b"));
    assert_eq!(list.nodes.len(), 2);
    assert_eq!(list.nodes[0].id, "node-a");
    assert_eq!(list.nodes[0].name, "node-a");
    assert_eq!(list.nodes[0].host.as_deref(), Some("10.0.0.5"));
    assert_eq!(list.nodes[0].api_port, Some(7390));
    assert!(list.nodes[0].online);
    assert_eq!(list.nodes[1].name, "Desk node");
    assert!(!list.nodes[1].online);

    let node = client.get_node("node-a").await.unwrap();
    assert_eq!(node.id, "node-a");
    assert!(node.active);

    let activated = client.activate_node("node-a").await.unwrap();
    assert!(activated.ok);
    assert_eq!(activated.active_node_id.as_deref(), Some("node-a"));

    let requests = recorded.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v1/nodes");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer secret-token")
    );
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].path, "/api/v1/nodes/node-a");
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path, "/api/v1/nodes/node-a/activate");
}

#[tokio::test]
async fn controller_http_errors_are_typed() {
    let (base_url, recorded) = mock_server(vec![Response::status(
        404,
        r#"{"detail":"Node not found"}"#,
    )])
    .await;

    let client = ControllerClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();
    let error = client.get_node("missing").await.unwrap_err();
    assert_eq!(error.code, ozma_protocol::codes::NOT_FOUND);
    assert_eq!(error.message, "Node not found");
    assert_eq!(error.details["status"], 404);

    let requests = recorded.lock().await;
    assert_eq!(requests[0].path, "/api/v1/nodes/missing");
}

#[tokio::test]
async fn controller_structured_error_codes_are_preserved() {
    let (base_url, _) = mock_server(vec![Response::status(
        503,
        r#"{"ok":false,"error":{"code":"transport.unavailable","message":"No route","retryable":true}}"#,
    )])
    .await;

    let client = ControllerClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();
    let error = client.list_nodes().await.unwrap_err();
    assert_eq!(error.code, "transport.unavailable");
    assert!(error.retryable);
}

#[tokio::test]
async fn node_transport_info_decodes_the_certificate() {
    let certificate =
        ozma_transport::SelfSignedCertificate::generate(&["localhost".to_string()]).unwrap();
    let der = certificate.certificate_der().to_vec();
    let fingerprint = certificate_fingerprint(&der);
    let (base_url, recorded) = mock_server(vec![Response::ok(
        json!({
            "endpoint": "127.0.0.1:4433",
            "fingerprint": fingerprint,
            "certificate_der_base64": STANDARD.encode(&der),
            "future": true
        })
        .to_string(),
    )])
    .await;

    let client = NodeApiClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();
    let fetched = client.transport_info().await.unwrap();
    assert_eq!(fetched.endpoint(), "127.0.0.1:4433");
    assert_eq!(fetched.fingerprint(), fingerprint);
    assert_eq!(fetched.certificate_der, der);

    let requests = recorded.lock().await;
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v1/transport");
}

#[tokio::test]
async fn node_transport_info_rejects_a_bad_fingerprint() {
    let certificate =
        ozma_transport::SelfSignedCertificate::generate(&["localhost".to_string()]).unwrap();
    let der = certificate.certificate_der().to_vec();
    let (base_url, _) = mock_server(vec![Response::ok(
        json!({
            "endpoint": "127.0.0.1:4433",
            "fingerprint": "00".repeat(32),
            "certificate_der_base64": STANDARD.encode(&der)
        })
        .to_string(),
    )])
    .await;

    let client = NodeApiClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();
    let error = client.transport_info().await.unwrap_err();
    assert_eq!(error.code, codes::CLIENT_CERTIFICATE);
    assert_eq!(error.details["fingerprint"], "00".repeat(32));
}

fn session_json(id: &str, state: SessionState) -> Value {
    let spec = SessionSpec {
        client_id: ClientId::new("client-test"),
        principal: "test".to_string(),
        tracks: Vec::new(),
        audio: Default::default(),
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        quality: None,
        ttl_seconds: 60,
    };
    let permissions = PermissionSet::from_iter([Permission::View, Permission::Control]);
    let mut session = Session::new(SessionId::new(id), spec, permissions, 1);
    session.state = state;
    serde_json::to_value(session).unwrap()
}

#[tokio::test]
async fn node_session_create_approve_start_and_get_use_the_real_api() {
    let offer = SessionOfferBuilder::new("sess-1")
        .token("session-token")
        .expires_at(1_700_000_000_000)
        .transport(
            TransportDescriptorBuilder::quic("127.0.0.1:4433")
                .certificate_fingerprint("00".repeat(32))
                .build(),
        )
        .build()
        .unwrap();

    let (base_url, recorded) = mock_server(vec![
        Response::status(
            201,
            json!({
                "session": session_json("sess-1", SessionState::Created),
                "offer": serde_json::to_value(&offer).unwrap(),
            })
            .to_string(),
        ),
        Response::ok(
            json!({ "session": session_json("sess-1", SessionState::Preparing) }).to_string(),
        ),
        Response::ok(
            json!({ "session": session_json("sess-1", SessionState::Streaming) }).to_string(),
        ),
        Response::ok(
            json!({ "session": session_json("sess-1", SessionState::Streaming) }).to_string(),
        ),
    ])
    .await;

    let spec = SessionSpec {
        client_id: ClientId::new("client-test"),
        principal: "ozma-client".to_string(),
        tracks: vec![TrackSpec {
            track_id: TrackId::new("video0"),
            kind: TrackKind::Capture,
            source_id: Some("/dev/video0".to_string()),
            display: None,
            format: None,
            mode: Some(DisplayMode::new(1920, 1080, 30)),
            hdr: false,
        }],
        audio: AudioSpec {
            enabled: true,
            mic: false,
            ..Default::default()
        },
        input: InputSpec {
            keyboard: true,
            pointer: PointerMode::Absolute,
            owner_only: true,
            ..Default::default()
        },
        privacy: PrivacyPolicy {
            consent: ozma_protocol::ConsentRequirement::Never,
            indicate: true,
            ..Default::default()
        },
        transport: TransportPreference::Quic,
        quality: None,
        ttl_seconds: 300,
    };

    let client = NodeApiClient::builder(&base_url)
        .token("secret-token")
        .no_proxy(true)
        .build()
        .unwrap();

    let (created, returned_offer) = client.create_session(&spec).await.unwrap();
    assert_eq!(created.id.as_str(), "sess-1");
    assert_eq!(created.state, SessionState::Created);
    assert_eq!(returned_offer.as_ref().unwrap().token, "session-token");

    let approved = client
        .approve_consent(&SessionId::new("sess-1"), "ozma-client")
        .await
        .unwrap();
    assert_eq!(approved.state, SessionState::Preparing);

    let started = client
        .start_session(&SessionId::new("sess-1"))
        .await
        .unwrap();
    assert_eq!(started.state, SessionState::Streaming);

    let fetched = client.get_session(&SessionId::new("sess-1")).await.unwrap();
    assert_eq!(fetched.id.as_str(), "sess-1");

    let requests = recorded.lock().await;
    assert_eq!(requests.len(), 4);

    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/api/v1/sessions");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer secret-token")
    );
    let create_body: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(create_body["client_id"], "client-test");
    assert_eq!(create_body["tracks"][0]["track_id"], "video0");
    assert_eq!(create_body["tracks"][0]["kind"], "capture");
    assert_eq!(create_body["tracks"][0]["source_id"], "/dev/video0");
    assert_eq!(create_body["tracks"][0]["mode"]["width"], 1920);
    assert_eq!(create_body["tracks"][0]["mode"]["height"], 1080);
    assert_eq!(create_body["tracks"][0]["mode"]["refresh_hz"], 30);
    assert_eq!(create_body["input"]["keyboard"], true);
    assert_eq!(create_body["input"]["pointer"], "absolute");
    assert_eq!(create_body["privacy"]["consent"], "never");
    assert_eq!(create_body["audio"]["enabled"], true);
    assert_eq!(create_body["transport"], "quic");

    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/api/v1/sessions/sess-1/approve");
    let approve_body: Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(approve_body, json!({ "by": "ozma-client" }));

    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path, "/api/v1/sessions/sess-1/start");
    assert_eq!(requests[2].body, "");

    assert_eq!(requests[3].method, "GET");
    assert_eq!(requests[3].path, "/api/v1/sessions/sess-1");
}

#[tokio::test]
async fn node_session_errors_and_bare_session_payloads_are_typed() {
    let (base_url, recorded) = mock_server(vec![
        Response::status(404, r#"{"detail":"Session not found"}"#),
        Response::ok(
            serde_json::to_string(&session_json("sess-2", SessionState::Created)).unwrap(),
        ),
    ])
    .await;

    let client = NodeApiClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();

    let error = client
        .start_session(&SessionId::new("missing"))
        .await
        .unwrap_err();
    assert_eq!(error.code, ozma_protocol::codes::NOT_FOUND);
    assert_eq!(error.message, "Session not found");

    let fetched = client.get_session(&SessionId::new("sess-2")).await.unwrap();
    assert_eq!(fetched.state, SessionState::Created);

    let requests = recorded.lock().await;
    assert_eq!(requests[0].path, "/api/v1/sessions/missing/start");
    assert_eq!(requests[1].path, "/api/v1/sessions/sess-2");
}

#[tokio::test]
async fn node_transport_info_rejects_invalid_base64() {
    let (base_url, _) = mock_server(vec![Response::ok(
        json!({
            "endpoint": "127.0.0.1:4433",
            "fingerprint": "abc",
            "certificate_der_base64": "not base64 !!!"
        })
        .to_string(),
    )])
    .await;

    let client = NodeApiClient::builder(&base_url)
        .no_proxy(true)
        .build()
        .unwrap();
    let error = client.transport_info().await.unwrap_err();
    assert_eq!(error.code, codes::CLIENT_CERTIFICATE);
}
