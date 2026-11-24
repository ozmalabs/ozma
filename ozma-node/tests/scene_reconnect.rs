// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Reconnect acceptance for the native scene profile.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_node::backend::SessionBackend;
use ozma_node::scene_replay::JsonlSceneProducer;
use ozma_node::transport::QuicRuntime;
use ozma_protocol::scene::{DisplayCapabilities, ImageResource, SceneReceiver};
use ozma_protocol::{
    ClientId, ControlMessage, NodeCapabilities, Permission, PermissionSet, SessionSpec, TrackId,
    TrackKind, TrackSpec, WireFormat,
};
use ozma_transport::{
    ChannelId, ChannelMap, QuicClient, QuicTransport, SessionTransport, TransportEvent,
};
use tokio::time::timeout;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nreconnect-fixture";

fn replay_producer() -> JsonlSceneProducer {
    let image = ImageResource { id: 99, png: PNG }.encode();
    let resource = serde_json::json!({
        "type": "resource",
        "id": 99,
        "payload_base64": base64::engine::general_purpose::STANDARD.encode(image),
    });
    let placement = serde_json::json!({
        "type": "scene_commit",
        "commit": { "revision": 4, "images": [
            { "node_id": 1, "resource_id": 99, "x": 31, "y": 47, "width": 80, "height": 40 }
        ] },
    });
    JsonlSceneProducer::from_jsonl(format!("{resource}\n{placement}\n").as_bytes(), 4096).unwrap()
}

fn spec() -> SessionSpec {
    SessionSpec {
        client_id: ClientId::new("reconnect-client"),
        principal: "reconnect-test".into(),
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
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        ttl_seconds: 60,
    }
}

async fn control(client: &mut QuicTransport) -> ControlMessage {
    loop {
        match client.recv().await {
            TransportEvent::Payload {
                channel: ChannelId::CONTROL,
                payload,
            } => {
                let message = serde_json::from_slice(&payload).unwrap();
                if !matches!(message, ControlMessage::Event { .. }) {
                    return message;
                }
            }
            TransportEvent::Closed { reason } => panic!("closed: {reason:?}"),
            TransportEvent::Error { error } => panic!("transport error: {error}"),
            _ => {}
        }
    }
}

async fn attach(
    address: SocketAddr,
    certificate: Vec<u8>,
    session: &ozma_protocol::Session,
    token: &str,
) -> QuicTransport {
    let mut client = QuicClient::connect(
        address,
        "localhost",
        certificate,
        ChannelMap::default_session(),
    )
    .await
    .unwrap();
    let hello = ControlMessage::Hello {
        session_id: session.id.clone(),
        token: token.to_owned(),
        wire_formats: vec![WireFormat::Json],
        display: Some(DisplayCapabilities {
            scene_versions: vec![1],
            ..Default::default()
        }),
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&hello).unwrap()),
        )
        .await
        .unwrap();
    let ack = control(&mut client).await;
    assert!(matches!(
        ack,
        ControlMessage::HelloAck {
            display: Some(_),
            ..
        }
    ));
    client
}

async fn receive_scene(client: &mut QuicTransport) -> SceneReceiver {
    let resource = timeout(Duration::from_secs(5), client.recv_resource())
        .await
        .unwrap()
        .unwrap();
    let image = ImageResource::decode(&resource).unwrap();
    assert_eq!(image.id, 99);
    loop {
        if let ControlMessage::SceneCommit { commit } = control(client).await {
            let snapshot = ozma_protocol::scene::SceneSnapshot {
                revision: 0,
                full_display: ozma_protocol::scene::VideoSurface {
                    id: 1,
                    channel: ChannelId::VIDEO.raw(),
                },
            };
            let mut receiver = SceneReceiver::new(snapshot, resource.len());
            assert!(!receiver.receive_resource(&resource).unwrap());
            assert_eq!(receiver.image(99), Some(PNG));
            assert_eq!(commit.revision, 4);
            assert_eq!(
                receiver.receive_commit(commit),
                ozma_protocol::scene::CommitStatus::Presented
            );
            assert_eq!(receiver.presented().images[0].y, 47);
            return receiver;
        }
    }
}

#[tokio::test]
async fn scene_reconnect_gets_fresh_resource_and_same_placement() {
    let backend = Arc::new(
        SessionBackend::new(
            NodeCapabilities::fake("scene-reconnect"),
            ConsentPolicy::for_machine(MachineClass::Server),
        )
        .with_scene_producer(Arc::new(replay_producer())),
    );
    let runtime = QuicRuntime::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = runtime.local_addr();
    let certificate = runtime.certificate_der().to_vec();
    backend.configure_transport(runtime.endpoint(), runtime.fingerprint());
    let runner = backend.clone();
    tokio::spawn(async move {
        let _ = ozma_node::transport::run(runtime, runner).await;
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let session = backend
        .manager()
        .create(spec(), PermissionSet::from_iter([Permission::View]), now)
        .unwrap();
    let session = backend.start_session(&session.id).unwrap();
    let offer = backend.session_offer(&session).unwrap();
    timeout(Duration::from_secs(10), async {
        let mut first = attach(address, certificate.clone(), &session, &offer.token).await;
        let first_scene = receive_scene(&mut first).await;
        assert_eq!(first_scene.presented().images[0].x, 31);
        wait_for_video(&mut first).await;
        drop(first);

        let mut second = attach(address, certificate, &session, &offer.token).await;
        let second_scene = receive_scene(&mut second).await;
        assert_eq!(second_scene.presented(), first_scene.presented());
        wait_for_video(&mut second).await;
    })
    .await
    .expect("scene reconnect timed out");
}

async fn wait_for_video(client: &mut QuicTransport) {
    loop {
        match client.recv().await {
            TransportEvent::Datagram {
                channel: ChannelId::VIDEO,
                payload,
            } => {
                assert!(!payload.is_empty());
                return;
            }
            TransportEvent::Closed { reason } => panic!("closed before video: {reason:?}"),
            TransportEvent::Error { error } => panic!("error before video: {error}"),
            _ => {}
        }
    }
}
