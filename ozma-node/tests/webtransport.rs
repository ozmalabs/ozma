// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! End-to-end WebTransport listener tests.
//!
//! A real loopback HTTP/3 listener and the native [`WebTransportClient`] prove
//! the browser-facing contract: the same channel framing as raw QUIC, a control
//! Hello/HelloAck over the bidirectional stream, resources over a unidirectional
//! stream, fragmented video over datagrams, typed errors for bad payloads, and
//! terminal close semantics.
//!
//! Real-browser verification (WebCodecs canvas playback) is out of scope and is
//! not claimed here.

#![cfg(feature = "webtransport")]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ozma_api::NodeBackend;
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_client_core::VideoAssembler;
use ozma_node::backend::SessionBackend;
use ozma_node::transport::WebTransportRuntime;
use ozma_protocol::media::VideoFragmentHeader;
use ozma_protocol::{
    codes, ClientId, ControlMessage, NodeCapabilities, Permission, PermissionSet, SessionSpec,
    TrackId, TrackKind, TrackSpec, WireFormat,
};
use ozma_transport::{
    ChannelId, ChannelMap, SessionTransport, TransportEvent, WebTransportClient,
    WebTransportServer, WebTransportSession,
};
use tokio::time::timeout;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// A connected client/server pair on loopback. The server is returned so it
/// stays alive for the duration of the test; dropping it would close the
/// endpoint.
async fn pair() -> (WebTransportServer, WebTransportSession, WebTransportSession) {
    let server = WebTransportServer::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let url = server.session_url();
    let hash = server.certificate_sha256();
    let client = tokio::spawn(async move {
        WebTransportClient::connect(&url, hash, ChannelMap::default_session()).await
    });

    let server_session = timeout(STEP_TIMEOUT, server.accept())
        .await
        .expect("server accept timed out")
        .expect("server accept failed");
    let client_session = timeout(STEP_TIMEOUT, client)
        .await
        .expect("client task timed out")
        .expect("client task panicked")
        .expect("client connect failed");

    (server, server_session, client_session)
}

async fn next_event(session: &mut WebTransportSession) -> TransportEvent {
    timeout(STEP_TIMEOUT, session.recv())
        .await
        .expect("timed out waiting for a transport event")
}

fn session_spec() -> SessionSpec {
    SessionSpec {
        client_id: ClientId::new("browser-1"),
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
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        ttl_seconds: 60,
    }
}

#[tokio::test]
async fn node_webtransport_session_authenticates_and_streams_video() {
    let backend = Arc::new(SessionBackend::new(
        NodeCapabilities::fake("webtransport-test"),
        ConsentPolicy::for_machine(MachineClass::Server),
    ));
    let runtime = WebTransportRuntime::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let endpoint = runtime.endpoint();
    let fingerprint = runtime.fingerprint().to_string();
    let certificate_sha256 = runtime.certificate_sha256();

    // The raw QUIC descriptor stays first and unchanged; the browser listener is
    // additive.
    backend.configure_transport("127.0.0.1:1", "quic-fixture");
    backend.configure_webtransport_with_certificate(
        endpoint.clone(),
        fingerprint.clone(),
        runtime.certificate_der(),
    );

    let server_backend = backend.clone();
    tokio::spawn(async move {
        if let Err(error) = ozma_node::transport::run_webtransport(runtime, server_backend).await {
            panic!("WebTransport listener exited: {error:#}");
        }
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let session = backend
        .manager()
        .create(
            session_spec(),
            PermissionSet::from_iter([Permission::View, Permission::Control]),
            now,
        )
        .unwrap();
    let session = backend.start_session(&session.id).unwrap();
    let offer = backend.session_offer(&session).unwrap();

    assert_eq!(offer.transports.len(), 2);
    assert_eq!(
        offer.transports[0].endpoint.as_deref(),
        Some("127.0.0.1:1"),
        "the native QUIC descriptor must stay first"
    );
    let webtransport = &offer.transports[1];
    assert_eq!(webtransport.mode, ozma_protocol::TransportMode::Quic);
    assert!(webtransport.secure);
    assert_eq!(webtransport.alpn.as_deref(), Some("h3"));
    assert_eq!(webtransport.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(
        webtransport.certificate_fingerprint.as_deref(),
        Some(fingerprint.as_str())
    );

    let mut client = WebTransportClient::connect(
        webtransport.endpoint.as_deref().unwrap(),
        certificate_sha256,
        ChannelMap::default_session(),
    )
    .await
    .expect("WebTransport client connect");

    let hello = ControlMessage::Hello {
        session_id: session.id.clone(),
        token: offer.token.clone(),
        wire_formats: vec![WireFormat::Json],
        display: None,
    };
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&hello).unwrap()),
        )
        .await
        .unwrap();

    loop {
        match next_event(&mut client).await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                let message: ControlMessage = serde_json::from_slice(&payload).unwrap();
                if let ControlMessage::HelloAck {
                    session: acknowledged,
                    ..
                } = message
                {
                    assert_eq!(acknowledged.id, session.id);
                    break;
                }
            }
            TransportEvent::Error { error } => panic!("control error before HelloAck: {error}"),
            TransportEvent::Closed { reason } => {
                panic!("closed before HelloAck: {reason:?}")
            }
            _ => continue,
        }
    }

    // The fake pipeline streams fragmented video on the datagram channel.
    loop {
        match next_event(&mut client).await {
            TransportEvent::Datagram { channel, payload } if channel == ChannelId::VIDEO => {
                assert!(!payload.is_empty(), "video fragment must not be empty");
                break;
            }
            TransportEvent::Error { error } => panic!("media error: {error}"),
            TransportEvent::Closed { reason } => panic!("closed before video: {reason:?}"),
            _ => continue,
        }
    }

    client.close().await.unwrap();
}

#[tokio::test]
async fn resource_stream_round_trips_bytes() {
    let (_server, server, client) = pair().await;
    let payload = Bytes::from(
        (0..5_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>(),
    );

    let sent = payload.clone();
    let sender = tokio::spawn(async move {
        client.send_resource(sent).await.unwrap();
        client
    });
    let received = timeout(STEP_TIMEOUT, server.recv_resource())
        .await
        .expect("resource receive timed out")
        .expect("resource receive failed");
    assert_eq!(received, payload);
    let _client = sender.await.unwrap();
}

#[tokio::test]
async fn video_datagrams_reassemble_with_the_shared_assembler() {
    let (_server, server, mut client) = pair().await;
    let data: Vec<u8> = (0..2_400).map(|index| (index % 253) as u8).collect();
    const FRAGMENT: usize = 1_000;
    let count = data.len().div_ceil(FRAGMENT) as u16;
    for (index, chunk) in data.chunks(FRAGMENT).enumerate() {
        let header = VideoFragmentHeader::new(true, 0, 9, index as u16, count);
        server
            .send(ChannelId::VIDEO, Bytes::from(header.encode_into(chunk)))
            .await
            .unwrap();
    }

    let mut assembler = VideoAssembler::default();
    let frame = loop {
        match next_event(&mut client).await {
            TransportEvent::Datagram { channel, payload } if channel == ChannelId::VIDEO => {
                if let Some(frame) = assembler.push(&payload).unwrap() {
                    break frame;
                }
            }
            TransportEvent::Error { error } => panic!("datagram error: {error}"),
            TransportEvent::Closed { reason } => panic!("closed before the frame: {reason:?}"),
            _ => continue,
        }
    };
    assert_eq!(frame.sequence, 9);
    assert!(frame.keyframe);
    assert_eq!(frame.data, data);
}

#[tokio::test]
async fn invalid_and_oversized_payloads_are_typed_errors() {
    let (_server, mut server, client) = pair().await;

    // Per-channel limit from the negotiated map (video: 1200 bytes).
    let error = client
        .send(ChannelId::VIDEO, Bytes::from(vec![0u8; 1_201]))
        .await
        .expect_err("oversized video datagram must be rejected");
    assert_eq!(error.code, codes::INVALID_REQUEST);

    // Unknown channel id.
    let error = client
        .send(ChannelId::new(99), Bytes::from_static(b"x"))
        .await
        .expect_err("unknown channel must be rejected");
    assert_eq!(error.code, codes::INVALID_REQUEST);

    // Control payload beyond the channel's u16 limit.
    let error = client
        .send(ChannelId::CONTROL, Bytes::from(vec![0u8; 70_000]))
        .await
        .expect_err("oversized control payload must be rejected");
    assert_eq!(error.code, codes::INVALID_REQUEST);

    // The session survives a rejected payload: a valid frame still arrives.
    client
        .send(ChannelId::CONTROL, Bytes::from_static(b"still alive"))
        .await
        .unwrap();
    let event = loop {
        match next_event(&mut server).await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                break payload;
            }
            TransportEvent::Error { .. } => continue,
            TransportEvent::Closed { reason } => panic!("closed after typed errors: {reason:?}"),
            _ => continue,
        }
    };
    assert_eq!(event.as_ref(), &b"still alive"[..]);
}

#[tokio::test]
async fn close_is_idempotent_and_recv_stays_closed() {
    let (_server, _server_session, mut client) = pair().await;
    client.close().await.unwrap();
    client.close().await.unwrap();
    assert!(client.is_closed());

    let first = next_event(&mut client).await;
    assert!(first.is_closed(), "expected Closed, got {first:?}");
    let second = next_event(&mut client).await;
    assert_eq!(first, second, "recv must keep returning the same Closed");
}
