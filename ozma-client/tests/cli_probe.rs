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
use ozma_protocol::{
    ClientId, ControlMessage, Participant, ParticipantRole, Permission, PermissionSet, Session,
    SessionId, SessionOffer, SessionSpec, SessionState,
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
    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let certificate = server.certificate_der().to_vec();
    let fingerprint = certificate_fingerprint(&certificate);
    let offer = offer_for(address, &fingerprint);

    let directory = PathBuf::from(format!(
        "/tmp/opencode/ozma-client-probe-{}",
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
        send_control(
            &transport,
            &ControlMessage::HelloAck {
                session: session_with(control_permissions()),
            },
        )
        .await;
        transport
            .send(ChannelId::VIDEO, Bytes::from_static(b"probe-video!"))
            .await
            .unwrap();

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
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(binary)
            .arg("probe")
            .arg("--offer")
            .arg(&offer_path)
            .arg("--cert")
            .arg(&cert_path)
            .arg("--timeout")
            .arg("5")
            .arg("--json")
            .output(),
    )
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

    let saw_release = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    assert!(saw_release, "probe did not send ReleaseAll");

    std::fs::remove_dir_all(&directory).ok();
}
