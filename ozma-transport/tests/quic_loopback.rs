// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end QUIC loopback: bind, connect, exchange a reliable control
//! message and an RFC 9221 datagram, then close cleanly.

use std::time::Duration;

use bytes::Bytes;
use ozma_protocol::TransportMode;
use ozma_transport::quic::{QuicClient, QuicServer};
use ozma_transport::{
    ChannelId, ChannelMap, CloseReason, SessionTransport, TransportEvent,
};
use tokio::time::timeout;

const OP_TIMEOUT: Duration = Duration::from_secs(5);

async fn recv(transport: &mut impl SessionTransport) -> TransportEvent {
    timeout(OP_TIMEOUT, transport.recv())
        .await
        .expect("transport receive timed out")
}

#[tokio::test]
async fn quic_loopback_exchanges_control_and_datagram_then_closes() {
    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).expect("bind QUIC server");
    let addr = server.local_addr().expect("server address");
    let certificate = server.certificate_der().to_vec();

    let server_task = tokio::spawn(async move {
        let mut transport = server.accept().await.expect("accept QUIC connection");
        assert_eq!(transport.mode(), TransportMode::Quic);

        let first = recv(&mut transport).await;
        let second = recv(&mut transport).await;
        let mut control_payload = None;
        let mut datagram_payload = None;
        for event in [first, second] {
            match event {
                TransportEvent::Payload {
                    channel: ChannelId::CONTROL,
                    payload,
                } => control_payload = Some(payload),
                TransportEvent::Datagram {
                    channel: ChannelId::VIDEO,
                    payload,
                } => datagram_payload = Some(payload),
                other => panic!("unexpected server event: {other:?}"),
            }
        }
        assert_eq!(control_payload.as_deref(), Some(b"hello control".as_ref()));
        assert_eq!(datagram_payload.as_deref(), Some(b"media frame".as_ref()));

        transport
            .send(ChannelId::CONTROL, Bytes::from_static(b"ack"))
            .await
            .expect("server echo");

        // The client closes after receiving the ack.
        let closed = recv(&mut transport).await;
        assert!(closed.is_closed(), "expected close, got {closed:?}");
        assert!(matches!(
            closed,
            TransportEvent::Closed {
                reason: CloseReason::PeerClosed | CloseReason::Normal
            }
        ));
        control_payload
    });

    let mut client = QuicClient::connect(
        addr,
        "localhost",
        &certificate,
        ChannelMap::default_session(),
    )
    .await
    .expect("connect QUIC client");

    client
        .send(ChannelId::CONTROL, Bytes::from_static(b"hello control"))
        .await
        .expect("send control");
    client
        .send(ChannelId::VIDEO, Bytes::from_static(b"media frame"))
        .await
        .expect("send datagram");

    let ack = recv(&mut client).await;
    match ack {
        TransportEvent::Payload {
            channel: ChannelId::CONTROL,
            payload,
        } => assert_eq!(payload.as_ref(), b"ack"),
        other => panic!("expected control ack, got {other:?}"),
    }

    client.close().await.expect("client close");
    assert!(client.is_closed());
    // recv is sticky after close.
    assert!(client.recv().await.is_closed());

    server_task.await.expect("server task panicked");
}

#[tokio::test]
async fn quic_rejects_a_wrong_pinned_certificate() {
    let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).expect("bind QUIC server");
    let addr = server.local_addr().expect("server address");
    // Generate a different certificate than the one the server presents.
    let other = ozma_transport::quic::SelfSignedCertificate::generate(&["localhost".to_string()])
        .expect("generate other certificate");

    let config = ozma_transport::quic::QuicConfig::default()
        .with_handshake_timeout(Duration::from_secs(2));
    let error = QuicClient::connect_with_config(
        addr,
        "localhost",
        other.certificate_der(),
        config,
        ChannelMap::default_session(),
    )
    .await
    .expect_err("a pinned mismatch must fail");
    assert_eq!(error.code, ozma_protocol::codes::TRANSPORT_UNAVAILABLE);

    // Keep the server alive until the client has failed.
    drop(server);
}
