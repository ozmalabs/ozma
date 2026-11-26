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
    WireFormat,
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
        quality: None,
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
                wire_format: WireFormat::Json,
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
                wire_format: WireFormat::Json,
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
                wire_format: WireFormat::Json,
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
                wire_format: WireFormat::Json,
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

#[tokio::test]
async fn protobuf_scene_snapshot_is_decoded_from_the_display_channel() {
    use ozma_display_protocol::{scene_convert, DisplayMessage};
    use ozma_protocol::scene::{DisplayProfile, SceneCommit, VideoSurface, SCENE_VERSION};

    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        assert_eq!(
            hello["data"]["wire_formats"],
            json!(["protobuf", "json"]),
            "the client must advertise protobuf and the JSON fallback"
        );
        // Resolve Display from the channel map instead of assuming its id.
        let display = transport
            .channel_map()
            .channel_for_kind(ChannelKind::Display)
            .expect("the session channel map includes Display");

        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(view_only_permissions()),
                wire_format: WireFormat::Protobuf,
                display: Some(DisplayProfile::Scene {
                    version: SCENE_VERSION,
                    initial_scene: ozma_protocol::scene::SceneSnapshot {
                        revision: 0,
                        full_display: VideoSurface {
                            id: 1,
                            channel: ChannelId::VIDEO.raw(),
                        },
                    },
                }),
            },
        )
        .await;

        let commit = SceneCommit {
            revision: 3,
            images: Vec::new(),
        };
        let snapshot =
            scene_convert::scene_commit_to_proto(&commit, ChannelId::VIDEO.raw()).unwrap();
        let frame = DisplayMessage::SceneSnapshot(snapshot).encode().unwrap();
        transport.send(display, Bytes::from(frame)).await.unwrap();

        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        let config = NativeClientConfig {
            scene_cache_bytes: Some(1024),
            ..NativeClientConfig::default()
        };
        let mut client = NativeSessionClient::connect_with_config(&offer, &certificate, config)
            .await
            .unwrap();
        assert_eq!(client.wire_format(), WireFormat::Protobuf);
        assert!(client.display_channel().is_some());
        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::ScenePresented { revision: 3 }
        );
        assert_eq!(client.scene().unwrap().presented().revision, 3);
        client.close().await.unwrap();
        server_task.await.unwrap();
    })
    .await
    .expect("protobuf scene exchange timed out");
}

const DISPLAY_DEFINITION: &[u8] = br#"
{
  "id": "org.ozma.test/native-display",
  "version": 1,
  "fields": [
    {"id": 1, "key": "org.ozma/test.temp", "type": "sint", "scale": 100},
    {"id": 2, "key": "org.ozma/test.online", "type": "bool"}
  ],
  "actions": [
    {
      "id": 7,
      "key": "org.ozma.scenario.activate",
      "params_schema": "{\"type\":\"object\"}",
      "permissions": ["scenario.control"]
    }
  ],
  "layout": {"type": "column", "children": []}
}
"#;

#[tokio::test]
async fn protobuf_definition_and_data_update_are_applied_from_the_display_channel() {
    use ozma_display_definition::DisplayDefinitionDocument;
    use ozma_display_protocol::{proto, DisplayEvent, DisplayMessage};

    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let document = DisplayDefinitionDocument::parse(DISPLAY_DEFINITION).expect("valid definition");
    let definition = document.to_proto().expect("compile definition");
    let definition_id = document.id().to_string();
    let data_update = proto::DataUpdate {
        definition_version: 1,
        values: vec![
            document
                .encode_field_value(1, &json!(65.5))
                .expect("encode sint field"),
            document
                .encode_field_value(2, &json!(true))
                .expect("encode bool field"),
        ],
        sequence: Some(9),
        timestamp_micros: None,
    };

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let _hello = recv_control(&mut transport).await;
        let display = transport
            .channel_map()
            .channel_for_kind(ChannelKind::Display)
            .expect("the session channel map includes Display");
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(view_only_permissions()),
                wire_format: WireFormat::Protobuf,
                display: None,
            },
        )
        .await;

        for message in [
            DisplayMessage::Definition(definition),
            DisplayMessage::DataUpdate(data_update),
        ] {
            transport
                .send(display, Bytes::from(message.encode().unwrap()))
                .await
                .unwrap();
        }

        // The device-to-node direction uses the same Display channel.
        let action = loop {
            match transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == display => {
                    break payload
                }
                TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
                TransportEvent::Error { error } => panic!("transport error: {error}"),
                TransportEvent::Closed { reason } => {
                    panic!("transport closed before the client action: {reason:?}")
                }
            }
        };
        match DisplayMessage::decode(&action).expect("client Display frame") {
            DisplayMessage::ActionInvoke(invoke) => {
                assert_eq!(invoke.action_id, 7);
                assert_eq!(invoke.idempotency_key, 42);
            }
            other => panic!("expected ActionInvoke, got {other:?}"),
        }

        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        let mut client = NativeSessionClient::connect_with_config(
            &offer,
            &certificate,
            NativeClientConfig::default(),
        )
        .await
        .unwrap();
        assert!(client.display_session().is_some());

        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::Display(DisplayEvent::DefinitionApplied {
                id: definition_id,
                version: 1,
            })
        );
        assert_eq!(
            client.recv_event().await.unwrap(),
            ClientEvent::Display(DisplayEvent::DataUpdated {
                definition_version: 1,
                changed_field_ids: vec![1, 2],
            })
        );
        let session = client.display_session().unwrap();
        assert_eq!(session.definition_version(), Some(1));
        assert_eq!(session.field_value(1), Some(&json!(6550)));
        assert_eq!(session.field_value(2), Some(&json!(true)));
        assert_eq!(session.last_data_sequence(), Some(9));

        client
            .send_display_message(DisplayMessage::ActionInvoke(proto::ActionInvoke {
                action_id: 7,
                params: Vec::new(),
                idempotency_key: 42,
                params_json: None,
                timestamp_micros: None,
            }))
            .await
            .unwrap();
        client.close().await.unwrap();
        server_task.await.unwrap();
    })
    .await
    .expect("protobuf definition exchange timed out");
}

#[tokio::test]
async fn malformed_and_unknown_display_frames_are_non_fatal() {
    use ozma_display_protocol::{proto, DisplayEvent, DisplayMessage, TAG_DATA_UPDATE};

    let (server, address, certificate, fingerprint) = bind_server();
    let offer = offer_for(address, &fingerprint);

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let _hello = recv_control(&mut transport).await;
        let display = transport
            .channel_map()
            .channel_for_kind(ChannelKind::Display)
            .expect("the session channel map includes Display");
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(view_only_permissions()),
                wire_format: WireFormat::Protobuf,
                display: None,
            },
        )
        .await;

        // A known tag with a truncated protobuf payload.
        transport
            .send(display, Bytes::from(vec![TAG_DATA_UPDATE, 0xff]))
            .await
            .unwrap();
        // A tag this client does not know.
        transport
            .send(display, Bytes::from_static(&[0x7f]))
            .await
            .unwrap();
        // The session must still process a valid frame afterwards.
        let invoke = proto::ActionInvoke {
            action_id: 3,
            params: Vec::new(),
            idempotency_key: 1,
            params_json: None,
            timestamp_micros: None,
        };
        transport
            .send(
                display,
                Bytes::from(DisplayMessage::ActionInvoke(invoke).encode().unwrap()),
            )
            .await
            .unwrap();

        let mut seen = Vec::new();
        drain_until_close(&mut transport, &mut seen).await;
        let _ = transport.close_with_reason(CloseReason::Normal).await;
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        let mut client = NativeSessionClient::connect_with_config(
            &offer,
            &certificate,
            NativeClientConfig::default(),
        )
        .await
        .unwrap();

        match client.recv_event().await.unwrap() {
            ClientEvent::UnknownControl { kind } => {
                assert!(
                    kind.starts_with("display.malformed"),
                    "unexpected kind: {kind}"
                );
            }
            other => panic!("expected a malformed Display control event, got {other:?}"),
        }
        match client.recv_event().await.unwrap() {
            ClientEvent::UnknownControl { kind } => {
                assert_eq!(kind, "display.unknown_tag.0x7f");
            }
            other => panic!("expected an unknown Display tag event, got {other:?}"),
        }
        assert!(!client.is_closed());

        match client.recv_event().await.unwrap() {
            ClientEvent::Display(DisplayEvent::Action(invoke)) => {
                assert_eq!(invoke.action_id, 3);
            }
            other => panic!("expected a Display action event, got {other:?}"),
        }

        client.close().await.unwrap();
        server_task.await.unwrap();
    })
    .await
    .expect("non-fatal Display frame exchange timed out");
}
