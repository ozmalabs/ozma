// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! M1 native-session acceptance over a real QUIC connection.
//!
//! This exercises the node-side contract end to end with the fake media
//! pipeline: token verification, Hello/HelloAck, a video datagram, and input
//! authorization. A view-only session must reject input.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_node::backend::SessionBackend;
use ozma_node::transport::QuicRuntime;
use ozma_protocol::{
    ClientId, ControlMessage, InputEvent, KeyModifiers, NodeCapabilities, Permission,
    PermissionSet, PointerButton, PointerMode, SessionEvent, SessionSpec, TrackId, TrackKind,
    TrackSpec,
};
use ozma_transport::{
    ChannelId, ChannelMap, QuicClient, QuicTransport, SessionTransport, TransportEvent,
};
use tokio::time::timeout;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

fn backend() -> Arc<SessionBackend> {
    Arc::new(SessionBackend::new(
        NodeCapabilities::fake("quic-test"),
        ConsentPolicy::for_machine(MachineClass::Server),
    ))
}

fn session_spec(control: bool) -> SessionSpec {
    SessionSpec {
        client_id: ClientId::new("client-1"),
        principal: "alice".into(),
        tracks: vec![TrackSpec {
            track_id: TrackId::new("video0"),
            kind: TrackKind::Capture,
            source_id: Some("fake0".into()),
            display: None,
            format: None,
            mode: None,
            hdr: false,
        }],
        audio: Default::default(),
        input: if control {
            ozma_protocol::InputSpec {
                keyboard: true,
                pointer: PointerMode::Absolute,
                ..Default::default()
            }
        } else {
            Default::default()
        },
        privacy: Default::default(),
        transport: Default::default(),
        ttl_seconds: 60,
    }
}

async fn wait_for_payload(
    client: &mut QuicTransport,
    channel: ChannelId,
    reliable: bool,
) -> Bytes {
    timeout(STEP_TIMEOUT, async {
        loop {
            match client.recv().await {
                TransportEvent::Payload { channel: got, payload } if got == channel && reliable => {
                    return payload;
                }
                TransportEvent::Datagram { channel: got, payload } if got == channel && !reliable => {
                    return payload;
                }
                TransportEvent::Closed { reason } => {
                    panic!("transport closed while waiting for channel {channel}: {reason:?}")
                }
                TransportEvent::Error { error } => panic!("transport error: {error}"),
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for payload")
}

async fn next_control(client: &mut QuicTransport) -> ControlMessage {
    loop {
        let payload = wait_for_payload(client, ChannelId::CONTROL, true).await;
        let message: ControlMessage = serde_json::from_slice(&payload).unwrap();
        if matches!(message, ControlMessage::Event { .. }) {
            continue;
        }
        return message;
    }
}

async fn connect(
    backend: &Arc<SessionBackend>,
    permissions: PermissionSet,
    session_spec: SessionSpec,
) -> (
    QuicTransport,
    ozma_protocol::Session,
    ozma_protocol::SessionOffer,
) {
    let runtime = QuicRuntime::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let addr = runtime.local_addr();
    let certificate = runtime.certificate_der().to_vec();
    backend.configure_transport(runtime.endpoint(), runtime.fingerprint());

    let server = backend.clone();
    tokio::spawn(async move {
        let _ = ozma_node::transport::run(runtime, server).await;
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let session = backend
        .manager()
        .create(session_spec, permissions, now)
        .unwrap();
    let session = backend.start_session(&session.id).unwrap();
    let offer = backend.session_offer(&session).unwrap();

    let client = QuicClient::connect(
        addr,
        "localhost",
        certificate,
        ChannelMap::default_session(),
    )
    .await
    .expect("QUIC client connect");

    (client, session, offer)
}

#[tokio::test]
async fn native_quic_session_authenticates_and_streams_video() {
    let backend = backend();
    let permissions = PermissionSet::from_iter([
        Permission::View,
        Permission::Control,
        Permission::Audio,
    ]);
    let mut events = backend.manager().subscribe();
    let (mut client, session, offer) = connect(&backend, permissions, session_spec(true)).await;

    let hello = ControlMessage::Hello {
        session_id: session.id.clone(),
        token: offer.token.clone(),
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&hello).unwrap()),
        )
        .await
        .unwrap();

    let ack = next_control(&mut client).await;
    match ack {
        ControlMessage::HelloAck { session: acknowledged } => {
            assert_eq!(acknowledged.id, session.id);
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }

    let video = wait_for_payload(&mut client, ChannelId::VIDEO, false).await;
    assert!(!video.is_empty(), "fake video datagram must not be empty");

    let input = ControlMessage::Input {
        event: InputEvent::Key {
            code: 0x04,
            down: true,
            modifiers: KeyModifiers::new(0),
        },
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&input).unwrap()),
        )
        .await
        .unwrap();
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(
                serde_json::to_vec(&ControlMessage::Ping { nonce: 7 }).unwrap(),
            ),
        )
        .await
        .unwrap();
    let pong = next_control(&mut client).await;
    assert_eq!(pong, ControlMessage::Pong { nonce: 7 });

    client.close().await.unwrap();

    let released = timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::InputReleased { session_id, .. }) if session_id == session.id => {
                    return true;
                }
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .expect("timed out waiting for input release on disconnect");
    assert!(released);
}

#[tokio::test]
async fn view_only_session_rejects_input() {
    let backend = backend();
    let permissions = PermissionSet::from_iter([Permission::View]);
    let (mut client, session, offer) = connect(&backend, permissions, session_spec(false)).await;

    let hello = ControlMessage::Hello {
        session_id: session.id.clone(),
        token: offer.token,
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&hello).unwrap()),
        )
        .await
        .unwrap();
    let ack = next_control(&mut client).await;
    assert!(matches!(ack, ControlMessage::HelloAck { .. }));

    let input = ControlMessage::Input {
        event: InputEvent::Button {
            button: PointerButton::Left,
            down: true,
        },
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&input).unwrap()),
        )
        .await
        .unwrap();

    let error = next_control(&mut client).await;
    match error {
        ControlMessage::Error { error } => {
            assert_eq!(error.code, ozma_protocol::error::codes::FORBIDDEN);
        }
        other => panic!("expected an input error, got {other:?}"),
    }

    client.close().await.unwrap();
}
