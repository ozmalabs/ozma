// SPDX-License-Identifier: AGPL-3.0-only
//! Real QUIC client/server tests with a fake node.
//!
//! The fake server uses `ozma_transport::QuicServer` directly, so the test
//! exercises the same wire contract as `ozma-node` without depending on it or
//! on capture hardware.

use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use ozma_client::{
    certificate_fingerprint, ClientEvent, NativeClientConfig, NativeSessionClient, NodeApiClient,
};
use ozma_protocol::{
    ApiError, ChannelKind, ClientId, ControlMessage, InputEvent, Participant, ParticipantRole,
    Permission, PermissionSet, Session, SessionId, SessionOffer, SessionSpec, SessionState,
};
use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
use ozma_transport::{
    ChannelId, CloseReason, QuicServer, QuicTransport, SessionTransport, TransportEvent,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn control_permissions() -> PermissionSet {
    PermissionSet::from_iter([Permission::View, Permission::Control])
}

fn view_only_permissions() -> PermissionSet {
    PermissionSet::from_iter([Permission::View])
}

fn session_with(permissions: PermissionSet) -> Session {
    let spec = SessionSpec {
        client_id: ClientId::new("client-test"),
        principal: "probe".to_string(),
        tracks: Vec::new(),
        audio: Default::default(),
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        ttl_seconds: 60,
    };
    let mut session = Session::new(SessionId::new("session-test"), spec, permissions, 1);
    session.state = SessionState::Streaming;
    session.started_at = Some(1);
    session.updated_at = 2;
    session.participants = vec![Participant {
        client_id: ClientId::new("client-test"),
        role: ParticipantRole::Controller,
        connected: true,
        last_seen: 2,
    }];
    session
}

fn offer_for(address: SocketAddr, fingerprint: &str) -> SessionOffer {
    let descriptor = TransportDescriptorBuilder::quic(address.to_string())
        .certificate_fingerprint(fingerprint)
        .build();
    SessionOfferBuilder::new("session-test")
        .token("token-test")
        .expires_at(1_700_000_000_000)
        .transport(descriptor)
        .build()
        .unwrap()
}

async fn recv_control(transport: &mut QuicTransport) -> Value {
    loop {
        match transport.recv().await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                return serde_json::from_slice(&payload).unwrap();
            }
            TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
            TransportEvent::Error { error } => panic!("transport error before control: {error}"),
            TransportEvent::Closed { reason } => {
                panic!("transport closed before control: {reason:?}")
            }
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

async fn send_raw_control(transport: &QuicTransport, value: &Value) {
    let payload = serde_json::to_vec(value).unwrap();
    transport
        .send(ChannelId::CONTROL, Bytes::from(payload))
        .await
        .unwrap();
}

async fn drain_until_close(transport: &mut QuicTransport, seen: &mut Vec<Value>) {
    loop {
        match transport.recv().await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                let value: Value = serde_json::from_slice(&payload).unwrap();
                let kind = value["type"].as_str().unwrap_or_default().to_string();
                seen.push(value);
                if kind == "bye" {
                    break;
                }
            }
            TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
            TransportEvent::Error { .. } | TransportEvent::Closed { .. } => break,
        }
    }
}

fn bind_server() -> (QuicServer, SocketAddr, Vec<u8>, String) {
    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let certificate = server.certificate_der().to_vec();
    let fingerprint = certificate_fingerprint(&certificate);
    (server, address, certificate, fingerprint)
}

#[tokio::test]
async fn hello_video_input_release_and_close() {
    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["data"]["session_id"], "session-test");
        assert_eq!(hello["data"]["token"], "token-test");

        // A HelloAck with unknown top-level and session fields must parse.
        let mut session = serde_json::to_value(session_with(control_permissions())).unwrap();
        session["future_session_field"] = json!(42);
        send_raw_control(
            &transport,
            &json!({
                "type": "hello_ack",
                "data": {
                    "session": session,
                    "future": true
                }
            }),
        )
        .await;

        transport
            .send(ChannelId::VIDEO, Bytes::from_static(b"encoded-video-frame"))
            .await
            .unwrap();

        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
        seen
    });

    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        NativeSessionClient::connect(&offer, &certificate),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(client.state(), Some(SessionState::Streaming));
    assert!(client.permissions().contains(Permission::Control));

    let event = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
        .await
        .unwrap()
        .unwrap();
    let packet = match event {
        ClientEvent::Media(packet) => packet,
        other => panic!("expected a video media packet, got {other:?}"),
    };
    assert_eq!(packet.channel, ChannelId::VIDEO);
    assert_eq!(packet.kind, ChannelKind::Video);
    assert_eq!(
        packet.track_id.as_ref().map(|track| track.as_str()),
        Some("video0")
    );
    assert_eq!(packet.payload, Bytes::from_static(b"encoded-video-frame"));

    client
        .send_input(&InputEvent::Key {
            code: 30,
            down: true,
            modifiers: Default::default(),
        })
        .await
        .unwrap();
    client.release_all().await.unwrap();
    client.close().await.unwrap();
    assert!(client.is_closed());

    let seen = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    assert!(seen
        .iter()
        .any(|message| message["type"] == "input" && message["data"]["event"]["type"] == "key"));
    assert!(seen
        .iter()
        .any(|message| message["data"]["event"]["type"] == "release_all"));
}

#[tokio::test]
async fn view_only_session_rejects_input_locally() {
    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let _hello = recv_control(&mut transport).await;
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(view_only_permissions()),
                display: None,
            },
        )
        .await;
        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
        seen
    });

    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        NativeSessionClient::connect(&offer, &certificate),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!client.permissions().contains(Permission::Control));

    let error = client
        .send_input(&InputEvent::Key {
            code: 30,
            down: true,
            modifiers: Default::default(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ozma_protocol::codes::FORBIDDEN);

    client.release_all().await.unwrap();
    client.close().await.unwrap();

    let seen = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    assert!(!seen.iter().any(|message| message["type"] == "input"));
}

#[tokio::test]
async fn a_server_input_error_is_relayed_and_blocks_later_input() {
    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let _hello = recv_control(&mut transport).await;
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(control_permissions()),
                display: None,
            },
        )
        .await;

        // The first input is accepted, then control is withdrawn server-side.
        let first = recv_control(&mut transport).await;
        assert_eq!(first["type"], "input");
        send_control(
            &transport,
            &ControlMessage::Error {
                error: ApiError::new(
                    ozma_protocol::codes::FORBIDDEN,
                    "The session grant is view-only",
                ),
            },
        )
        .await;

        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
        seen
    });

    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        NativeSessionClient::connect(&offer, &certificate),
    )
    .await
    .unwrap()
    .unwrap();

    client
        .send_input(&InputEvent::Key {
            code: 30,
            down: true,
            modifiers: Default::default(),
        })
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match client.recv_event().await.unwrap() {
                ClientEvent::Error { error } => return error,
                ClientEvent::Closed { reason } => panic!("closed instead of an error: {reason:?}"),
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(event.code, ozma_protocol::codes::FORBIDDEN);
    assert_eq!(client.server_error().unwrap().code, event.code);

    let later = client
        .send_input(&InputEvent::Key {
            code: 31,
            down: true,
            modifiers: Default::default(),
        })
        .await
        .unwrap_err();
    assert_eq!(later.code, ozma_protocol::codes::FORBIDDEN);
    assert_eq!(later.message, "The session grant is view-only");

    client.close().await.unwrap();

    let seen = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    assert!(!seen.is_empty());
}

#[tokio::test]
async fn a_wrong_certificate_is_rejected_before_connecting() {
    let (server, address, _certificate, fingerprint) = bind_server();
    drop(server);
    let offer = offer_for(address, &fingerprint);
    let other =
        ozma_transport::SelfSignedCertificate::generate(&["localhost".to_string()]).unwrap();

    let error = NativeSessionClient::connect(&offer, other.certificate_der())
        .await
        .unwrap_err();
    assert_eq!(error.code, ozma_client::codes::CLIENT_CERTIFICATE);
    assert_eq!(error.details["actual"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn an_offer_without_quic_is_rejected() {
    let offer = SessionOfferBuilder::new("session-test")
        .token("token-test")
        .expires_at(1)
        .transport(TransportDescriptorBuilder::loopback().build())
        .build()
        .unwrap();
    let error = NativeSessionClient::connect(&offer, [0u8; 1])
        .await
        .unwrap_err();
    assert_eq!(error.code, ozma_protocol::codes::TRANSPORT_UNAVAILABLE);
}

/// A one-shot HTTP server for `GET /api/v1/transport`.
async fn node_transport_api(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn connect_with_node_api_fetches_the_der_certificate() {
    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);
    let node_api = node_transport_api(
        json!({
            "endpoint": address.to_string(),
            "fingerprint": fingerprint,
            "certificate_der_base64": STANDARD.encode(&certificate),
        })
        .to_string(),
    )
    .await;

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        assert_eq!(hello["type"], "hello");
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(control_permissions()),
                display: None,
            },
        )
        .await;
        transport
            .send(ChannelId::VIDEO, Bytes::from_static(b"via-node-api"))
            .await
            .unwrap();
        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
    });

    let node_client = NodeApiClient::builder(node_api)
        .no_proxy(true)
        .build()
        .unwrap();
    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        NativeSessionClient::connect_with_node_api(&offer, &node_client, NativeClientConfig::new()),
    )
    .await
    .unwrap()
    .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
        .await
        .unwrap()
        .unwrap();
    match event {
        ClientEvent::Media(packet) => {
            assert_eq!(packet.payload, Bytes::from_static(b"via-node-api"))
        }
        other => panic!("expected media, got {other:?}"),
    }
    client.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn scene_client_receives_assets_and_presents_ordered_updates() {
    use ozma_protocol::scene::{
        DisplayCapabilities, ImagePlacement, ImageResource, SceneCommit, SCENE_VERSION,
    };

    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let certificate = server.certificate_der().to_vec();
    let offer = offer_for(
        server.local_addr().unwrap(),
        &certificate_fingerprint(&certificate),
    );
    let session = session_with(view_only_permissions());
    let png = b"\x89PNG\r\n\x1a\nfixture";
    let producer = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        let capabilities: DisplayCapabilities =
            serde_json::from_value(hello["data"]["display"].clone()).unwrap();
        assert_eq!(capabilities.scene_versions, vec![SCENE_VERSION]);
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session,
                display: Some(capabilities.select(ChannelId::VIDEO.raw())),
            },
        )
        .await;

        for (revision, y) in [(1, 40), (2, 10)] {
            send_control(
                &transport,
                &ControlMessage::SceneCommit {
                    commit: SceneCommit {
                        revision,
                        images: vec![ImagePlacement {
                            node_id: 1,
                            resource_id: 7,
                            x: 20,
                            y,
                            width: 320,
                            height: 180,
                        }],
                    },
                },
            )
            .await;
            let _ = recv_control(&mut transport).await;
            if revision == 1 {
                transport
                    .send_resource(Bytes::from(ImageResource { id: 7, png }.encode()))
                    .await
                    .unwrap();
                let _ = recv_control(&mut transport).await;
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        let config = NativeClientConfig {
            scene_cache_bytes: Some(png.len()),
            ..NativeClientConfig::default()
        };
        let mut client = NativeSessionClient::connect_with_config(&offer, &certificate, config)
            .await
            .unwrap();
        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::ScenePending
        );
        assert_eq!(client.scene().unwrap().presented().revision, 0);
        client.send_ping(1).await.unwrap();
        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::ScenePresented { revision: 1 }
        );
        assert_eq!(client.scene().unwrap().image(7), Some(png.as_slice()));
        client.send_ping(2).await.unwrap();
        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::ScenePresented { revision: 2 }
        );
        assert_eq!(client.scene().unwrap().presented().images[0].y, 10);
        client.send_ping(3).await.unwrap();
        producer.await.unwrap();
    })
    .await
    .expect("scene exchange timed out");
}
