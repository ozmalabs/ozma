// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end test of the `ozma-client probe` headless acceptance harness.
//!
//! Starts a fake QUIC node in-process, writes an offer and DER certificate to
//! a temporary directory, then runs the real binary as a child process.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use ozma_client::certificate_fingerprint;
use ozma_protocol::scene::{DisplayCapabilities, ImagePlacement, ImageResource, SceneCommit};
use ozma_protocol::{
    ClientId, ControlMessage, Participant, ParticipantRole, Permission, PermissionSet, Session,
    SessionId, SessionOffer, SessionSpec, SessionState, WireFormat,
};
use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
use ozma_transport::{
    ChannelId, CloseReason, QuicServer, QuicTransport, SessionTransport, TransportEvent,
};
use serde_json::Value;

fn control_permissions() -> PermissionSet {
    PermissionSet::from_iter([Permission::View, Permission::Control])
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
async fn probe_cli_connects_receives_video_and_exits_cleanly() {
    probe_cli(false).await;
}

#[tokio::test]
async fn probe_cli_reports_scene_readiness() {
    probe_cli(true).await;
}

async fn probe_cli(scene: bool) {
    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let certificate = server.certificate_der().to_vec();
    let fingerprint = certificate_fingerprint(&certificate);
    let offer = offer_for(address, &fingerprint);

    let directory = PathBuf::from(format!(
        "/tmp/opencode/ozma-client-probe-{}-{scene}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let offer_path = directory.join("offer.json");
    let cert_path = directory.join("node.der");
    std::fs::write(&offer_path, serde_json::to_vec_pretty(&offer).unwrap()).unwrap();
    std::fs::write(&cert_path, &certificate).unwrap();

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.unwrap();
        let hello = recv_control(&mut transport).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["data"]["token"], "token-test");
        let display = if scene {
            let capabilities: DisplayCapabilities =
                serde_json::from_value(hello["data"]["display"].clone()).unwrap();
            Some(capabilities.select(ChannelId::VIDEO.raw()))
        } else {
            assert!(hello["data"].get("display").is_none());
            None
        };
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(control_permissions()),
                wire_format: WireFormat::Json,
                display,
            },
        )
        .await;
        transport
            .send(ChannelId::VIDEO, Bytes::from_static(b"probe-video!"))
            .await
            .unwrap();

        if scene {
            let resource = ImageResource {
                id: 7,
                png: b"\x89PNG\r\n\x1a\nfixture",
            }
            .encode();
            transport
                .send_resource(Bytes::from(resource))
                .await
                .unwrap();
            send_control(
                &transport,
                &ControlMessage::SceneCommit {
                    commit: SceneCommit {
                        revision: 2,
                        images: vec![ImagePlacement {
                            node_id: 1,
                            resource_id: 7,
                            x: 0,
                            y: 10,
                            width: 320,
                            height: 180,
                        }],
                    },
                },
            )
            .await;
        }

        let mut saw_release = false;
        loop {
            match transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let value: Value = serde_json::from_slice(&payload).unwrap();
                    if value["type"] == "input" && value["data"]["event"]["type"] == "release_all" {
                        saw_release = true;
                    }
                    if value["type"] == "bye" {
                        break;
                    }
                }
                TransportEvent::Closed { .. } | TransportEvent::Error { .. } => break,
                _ => continue,
            }
        }
        let _ = transport.close_with_reason(CloseReason::Normal).await;
        saw_release
    });

    let binary = env!("CARGO_BIN_EXE_ozma-client");
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["probe", "--offer"])
        .arg(&offer_path)
        .arg("--cert")
        .arg(&cert_path)
        .args(["--timeout", "5", "--json"]);
    if scene {
        command.args(["--scene-revision", "2"]);
    }
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("probe timed out")
        .expect("run probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "probe failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let report: Value = serde_json::from_str(stdout.trim()).expect("probe JSON output");
    assert_eq!(report["session_id"], "session-test");
    assert_eq!(report["state"], "streaming");
    assert_eq!(report["video"]["payload_len"], 12);
    assert_eq!(report["video"]["kind"], "video");
    assert_eq!(report["video"]["channel"], 2);

    assert!(report["video"]["ready_ms"].is_u64());
    if scene {
        assert_eq!(report["scene"]["revision"], 2);
        assert!(report["scene"]["ready_ms"].is_u64());
    } else {
        assert!(report["scene"].is_null());
    }

    let saw_release = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    assert!(saw_release, "probe did not send ReleaseAll");

    std::fs::remove_dir_all(&directory).ok();
}
