// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end test of `ozma-client connect`.
//!
//! A tiny in-process HTTP server serves both the controller node API and the
//! node session API, and a fake QUIC node delivers one video datagram. The real
//! binary is run as a child process, proving the M1 control-plane loop:
//! discover + activate, create session, approve consent, start, fetch
//! transport, receive video, release input, and exit.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use ozma_client::certificate_fingerprint;
use ozma_protocol::{
    ClientId, ControlMessage, Permission, PermissionSet, Session, SessionId, SessionSpec,
    SessionState, WireFormat,
};
use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
use ozma_transport::{
    ChannelId, CloseReason, QuicServer, QuicTransport, SessionTransport, TransportEvent,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const SESSION_ID: &str = "sess-connect";
const VIDEO_PAYLOAD: &[u8] = b"connect-video";

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Clone)]
struct State {
    api_port: u16,
    quic_endpoint: SocketAddr,
    fingerprint: String,
    certificate_der: Vec<u8>,
    offer: Value,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

fn session_in(state: SessionState) -> Session {
    let spec = SessionSpec {
        client_id: ClientId::new("ozma-client"),
        principal: "ozma-client".to_string(),
        tracks: Vec::new(),
        audio: Default::default(),
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        quality: None,
        ttl_seconds: 300,
    };
    let permissions = PermissionSet::from_iter([Permission::View, Permission::Control]);
    let mut session = Session::new(SessionId::new(SESSION_ID), spec, permissions, 1);
    session.state = state;
    session.updated_at = 2;
    if state.is_active() || state == SessionState::Streaming {
        session.started_at = Some(2);
    }
    session
}

fn session_body(state: SessionState) -> String {
    json!({ "session": session_in(state) }).to_string()
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

async fn handle(mut socket: TcpStream, state: Arc<State>) {
    let request = read_request(&mut socket).await;
    let (status, body) = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/v1/nodes/node-1") => (
            200,
            json!({
                "id": "node-1",
                "name": "Test node",
                "host": "127.0.0.1",
                "api_port": state.api_port,
                "online": true,
                "capabilities": ["video", "audio"],
            })
            .to_string(),
        ),
        ("POST", "/api/v1/nodes/node-1/activate") => (
            200,
            json!({ "ok": true, "active_node_id": "node-1" }).to_string(),
        ),
        ("POST", "/api/v1/sessions") => (
            201,
            json!({
                "session": session_in(SessionState::PendingConsent),
                "offer": state.offer,
            })
            .to_string(),
        ),
        ("POST", "/api/v1/sessions/sess-connect/approve") => {
            (200, session_body(SessionState::Preparing))
        }
        ("POST", "/api/v1/sessions/sess-connect/start") => {
            (200, session_body(SessionState::Streaming))
        }
        ("GET", "/api/v1/transport") => (
            200,
            json!({
                "endpoint": state.quic_endpoint.to_string(),
                "fingerprint": state.fingerprint,
                "certificate_der_base64": STANDARD.encode(&state.certificate_der),
            })
            .to_string(),
        ),
        _ => (404, r#"{"detail":"not found"}"#.to_string()),
    };

    state.requests.lock().await.push(request);
    write_response(&mut socket, status, &body).await;
}

async fn write_response(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.shutdown().await.ok();
}

async fn recv_control(transport: &mut QuicTransport) -> Value {
    loop {
        match transport.recv().await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                return serde_json::from_slice(&payload).unwrap();
            }
            TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
            TransportEvent::Error { error } => panic!("transport error: {error}"),
            TransportEvent::Closed { reason } => panic!("closed early: {reason:?}"),
        }
    }
}

async fn send_control(transport: &QuicTransport, message: &ControlMessage) {
    let payload = serde_json::to_vec(message).unwrap();
    transport
        .send(ChannelId::CONTROL, Bytes::from(payload))
        .await
        .unwrap();
}

#[tokio::test]
async fn connect_cli_creates_starts_receives_video_and_exits() {
    let quic_server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let quic_endpoint = quic_server.local_addr().unwrap();
    let certificate_der = quic_server.certificate_der().to_vec();
    let fingerprint = certificate_fingerprint(&certificate_der);

    let offer = SessionOfferBuilder::new(SESSION_ID)
        .token("connect-token")
        .expires_at(1_700_000_000_000)
        .transport(
            TransportDescriptorBuilder::quic(quic_endpoint.to_string())
                .certificate_fingerprint(fingerprint.clone())
                .build(),
        )
        .build()
        .unwrap();

    let quic_task = tokio::spawn(async move {
        let mut transport = quic_server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["data"]["token"], "connect-token");
        assert_eq!(hello["data"]["session_id"], SESSION_ID);

        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_in(SessionState::Streaming),
                wire_format: WireFormat::Json,
                display: None,
            },
        )
        .await;
        transport
            .send(ChannelId::VIDEO, Bytes::from_static(VIDEO_PAYLOAD))
            .await
            .unwrap();

        let mut saw_release = false;
        let mut saw_bye = false;
        loop {
            match transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let value: Value = serde_json::from_slice(&payload).unwrap();
                    if value["type"] == "input" && value["data"]["event"]["type"] == "release_all" {
                        saw_release = true;
                    }
                    if value["type"] == "bye" {
                        saw_bye = true;
                        break;
                    }
                }
                TransportEvent::Closed { .. } | TransportEvent::Error { .. } => break,
                _ => continue,
            }
        }
        let _ = transport.close_with_reason(CloseReason::Normal).await;
        saw_release && saw_bye
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_port = listener.local_addr().unwrap().port();
    let state = Arc::new(State {
        api_port,
        quic_endpoint,
        fingerprint,
        certificate_der,
        offer: serde_json::to_value(&offer).unwrap(),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let http_state = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let state = http_state.clone();
            tokio::spawn(handle(socket, state));
        }
    });

    let directory = PathBuf::from(format!(
        "/tmp/opencode/ozma-client-connect-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let video_path = directory.join("first-frame.h264");

    let binary = env!("CARGO_BIN_EXE_ozma-client");
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("connect")
        .arg("--controller")
        .arg(format!("http://127.0.0.1:{api_port}"))
        .args(["--controller-token", "secret-token"])
        .args(["--node-id", "node-1"])
        .args(["--source", "/dev/video-test"])
        .args(["--width", "1280", "--height", "720", "--fps", "60"])
        .arg("--audio")
        .arg("--save-video")
        .arg(&video_path)
        .args(["--timeout", "5", "--json"])
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("connect timed out")
        .expect("run connect");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "connect failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: Value = serde_json::from_str(stdout.trim()).expect("connect JSON output");
    assert_eq!(report["node"]["id"], "node-1");
    assert_eq!(report["session_id"], SESSION_ID);
    assert_eq!(report["state"], "streaming");
    assert_eq!(report["video"]["payload_len"], VIDEO_PAYLOAD.len());
    assert_eq!(report["video"]["kind"], "video");
    assert_eq!(report["video"]["channel"], 2);

    let saved = std::fs::read(&video_path).unwrap();
    assert_eq!(saved, VIDEO_PAYLOAD);

    let requests = state.requests.lock().await;
    assert_eq!(requests.len(), 6, "requests: {requests:?}");

    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v1/nodes/node-1");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer secret-token")
    );

    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/api/v1/nodes/node-1/activate");

    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path, "/api/v1/sessions");
    assert_eq!(
        requests[2].authorization.as_deref(),
        Some("Bearer secret-token")
    );
    let create: Value = serde_json::from_str(&requests[2].body).unwrap();
    assert_eq!(create["client_id"], "ozma-client");
    assert_eq!(create["tracks"][0]["kind"], "capture");
    assert_eq!(create["tracks"][0]["source_id"], "/dev/video-test");
    assert_eq!(create["tracks"][0]["mode"]["width"], 1280);
    assert_eq!(create["tracks"][0]["mode"]["height"], 720);
    assert_eq!(create["tracks"][0]["mode"]["refresh_hz"], 60);
    assert_eq!(create["audio"]["enabled"], true);
    assert_eq!(create["input"]["keyboard"], true);
    assert_eq!(create["input"]["pointer"], "absolute");
    assert_eq!(create["privacy"]["consent"], "never");

    assert_eq!(requests[3].method, "POST");
    assert_eq!(requests[3].path, "/api/v1/sessions/sess-connect/approve");
    let approve: Value = serde_json::from_str(&requests[3].body).unwrap();
    assert_eq!(approve, json!({ "by": "ozma-client" }));

    assert_eq!(requests[4].method, "POST");
    assert_eq!(requests[4].path, "/api/v1/sessions/sess-connect/start");

    assert_eq!(requests[5].method, "GET");
    assert_eq!(requests[5].path, "/api/v1/transport");

    let saw_release = tokio::time::timeout(Duration::from_secs(2), quic_task)
        .await
        .expect("QUIC node did not finish")
        .expect("QUIC node task panicked");
    assert!(saw_release, "connect did not release input and send Bye");

    std::fs::remove_dir_all(&directory).ok();
}
