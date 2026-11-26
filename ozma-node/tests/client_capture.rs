// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Browser camera and microphone uplink over the real native QUIC listener.
//!
//! The tests encode real H.264 with the software encoder and real Opus with
//! libopus, so the node decodes exactly what a browser would send. No browser
//! or wasm runtime is involved.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use ozma_api::{CaptureReport, ClientCaptureSink, NodeBackend};
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_client_core::VideoAssembler;
use ozma_media::{Encoder as _, Frame, FrameFormat};
use ozma_node::backend::SessionBackend;
use ozma_node::client_capture::{
    ClientCaptureHub, BROWSER_CAMERA_TRACK, CLIENT_CAPTURE_REJECTED, MAX_CAMERA_ACCESS_UNIT,
};
use ozma_node::transport::QuicRuntime;
use ozma_protocol::media::VideoFragmentHeader;
use ozma_protocol::{
    AudioFormat, AudioSpec, ChannelKind, ClientId, Codec, ControlMessage, MediaFormat,
    NodeCapabilities, Permission, PermissionSet, SessionSpec, TrackId, TrackKind, TrackSpec,
    WireFormat,
};
use ozma_transport::{
    ChannelId, ChannelMap, QuicClient, QuicTransport, SessionTransport, TransportEvent,
};
use tokio::time::timeout;
use tower::ServiceExt;

const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const VIDEO_FRAGMENT_PAYLOAD: usize = 1_100;

fn backend() -> Arc<SessionBackend> {
    Arc::new(SessionBackend::new(
        NodeCapabilities::fake("client-capture-test"),
        ConsentPolicy::for_machine(MachineClass::Server),
    ))
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
        audio: AudioSpec {
            enabled: true,
            mic: true,
            format: AudioFormat::opus(),
        },
        input: Default::default(),
        privacy: Default::default(),
        transport: Default::default(),
        quality: None,
        ttl_seconds: 60,
    }
}

async fn connect(
    backend: &Arc<SessionBackend>,
) -> (
    QuicTransport,
    ozma_protocol::Session,
    ozma_protocol::SessionOffer,
) {
    let runtime = QuicRuntime::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = runtime.local_addr();
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
    let permissions = PermissionSet::from_iter([
        Permission::View,
        Permission::Control,
        Permission::Audio,
        Permission::Mic,
    ]);
    let session = backend
        .manager()
        .create(session_spec(), permissions, now)
        .unwrap();
    let session = backend.start_session(&session.id).unwrap();
    let offer = backend.session_offer(&session).unwrap();

    let mut client = QuicClient::connect(
        address,
        "localhost",
        certificate,
        ChannelMap::default_session(),
    )
    .await
    .expect("QUIC client connect");

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
    let ack = next_control(&mut client).await;
    assert!(
        matches!(ack, ControlMessage::HelloAck { .. }),
        "expected HelloAck, got {ack:?}"
    );

    (client, session, offer)
}

async fn next_control(client: &mut QuicTransport) -> ControlMessage {
    timeout(STEP_TIMEOUT, async {
        loop {
            match client.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let message: ControlMessage = serde_json::from_slice(&payload).unwrap();
                    if matches!(message, ControlMessage::Event { .. }) {
                        continue;
                    }
                    return message;
                }
                TransportEvent::Closed { reason } => {
                    panic!("transport closed while waiting for control: {reason:?}")
                }
                TransportEvent::Error { error } => panic!("transport error: {error}"),
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for control")
}

async fn send_control(client: &QuicTransport, message: ControlMessage) {
    client
        .send(
            ChannelId::CONTROL,
            Bytes::from(serde_json::to_vec(&message).unwrap()),
        )
        .await
        .unwrap();
}

fn channel(client: &QuicTransport, kind: ChannelKind) -> ChannelId {
    client
        .channel_map()
        .channel_for_kind(kind)
        .unwrap_or_else(|| panic!("negotiated map has no {kind:?} channel"))
}

/// Encode one real Annex-B H.264 IDR access unit with the software encoder.
async fn camera_access_unit(width: u32, height: u32) -> Vec<u8> {
    let format = FrameFormat::rgb24(width, height);
    let mut encoder = ozma_encode::H264SoftwareEncoder::new(
        format,
        MediaFormat::new(Codec::H264, width, height, 30, 2_000),
    )
    .expect("software H.264 encoder");
    let mut rgb = vec![0u8; (width * height * 3) as usize];
    for (index, byte) in rgb.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    let frame = Frame::with_data(format, 0, 0, true, rgb).unwrap();
    let encoded = encoder.encode(frame).await.expect("encode camera frame");
    assert!(encoded.is_keyframe(), "first software frame must be an IDR");
    encoded.data().to_vec()
}

async fn send_camera_frame(client: &QuicTransport, channel: ChannelId, data: &[u8], sequence: u64) {
    let count = data.len().div_ceil(VIDEO_FRAGMENT_PAYLOAD).max(1) as u16;
    for (index, chunk) in data.chunks(VIDEO_FRAGMENT_PAYLOAD).enumerate() {
        let datagram =
            VideoFragmentHeader::new(true, 0, sequence, index as u16, count).encode_into(chunk);
        client
            .send(channel, Bytes::from(datagram))
            .await
            .expect("send camera fragment");
    }
}

async fn wait_for_report(
    hub: &ClientCaptureHub,
    predicate: impl Fn(&CaptureReport) -> bool,
) -> CaptureReport {
    timeout(STEP_TIMEOUT, async {
        loop {
            let report = hub.report();
            if predicate(&report) {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out waiting for capture counters")
}

fn capture_router(backend: &Arc<SessionBackend>) -> (axum::Router, Arc<dyn ClientCaptureSink>) {
    let dynamic: Arc<dyn NodeBackend> = backend.clone();
    let sink: Arc<dyn ClientCaptureSink> = backend.clone();
    (
        ozma_api::router::router_with_sinks(dynamic, None, Some(sink.clone()), None),
        sink,
    )
}

#[tokio::test]
async fn camera_channel_decodes_a_real_h264_access_unit_and_serves_http() {
    let backend = backend();
    let (client, session, offer) = connect(&backend).await;

    // The offer carries the camera channel without moving existing ids.
    let camera_descriptor = offer
        .transports
        .iter()
        .flat_map(|transport| transport.channels.iter())
        .find(|descriptor| descriptor.kind == ChannelKind::Camera)
        .expect("offer must include the camera channel");
    assert_eq!(
        camera_descriptor.track_id,
        Some(TrackId::new(BROWSER_CAMERA_TRACK))
    );
    assert!(!camera_descriptor.reliable);
    assert_eq!(camera_descriptor.max_payload, 1200);

    send_control(
        &client,
        ControlMessage::CaptureStart {
            video: true,
            audio: false,
        },
    )
    .await;

    let camera_channel = channel(&client, ChannelKind::Camera);
    let hub = backend.client_capture(&session.id);
    wait_for_report(&hub, |report| report.video.started).await;

    let (width, height) = (64, 64);
    let access_unit = camera_access_unit(width, height).await;
    send_camera_frame(&client, camera_channel, &access_unit, 0).await;

    let report = wait_for_report(&hub, |report| report.video.frames >= 1).await;
    assert_eq!(report.video.width, width);
    assert_eq!(report.video.height, height);
    assert_eq!(report.video.codec, "h264");
    assert_eq!(report.video.decode_errors, 0);
    assert_eq!(report.video.last_sequence, 0);
    assert_eq!(report.video.packets, 1);
    assert!(report.video.bytes >= access_unit.len() as u64);

    let (app, _sink) = capture_router(&backend);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sessions/{}/capture", session.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["video"]["started"], true);
    assert_eq!(json["video"]["frames"], 1);
    assert_eq!(json["video"]["width"], width);
    assert_eq!(json["video"]["height"], height);
    assert_eq!(json["video"]["codec"], "h264");
    assert_eq!(json["video"]["last_sequence"], 0);
    assert_eq!(json["audio"]["started"], false);

    // The PPM is the proof artifact: P6 header and exact RGB byte count.
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sessions/{}/capture/frame.ppm", session.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/x-portable-pixmap"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let header = format!("P6\n{width} {height}\n255\n");
    assert!(body.starts_with(header.as_bytes()));
    assert_eq!(
        body.len(),
        header.len() + (width * height * 3) as usize,
        "PPM must contain one RGB triple per pixel"
    );

    client.close().await.unwrap();
}

#[tokio::test]
async fn microphone_channel_decodes_a_real_opus_packet() {
    use ozma_audio::{AudioEncoder, AudioFrame, EncoderConfig, OpusEncoder};

    let backend = backend();
    let (client, session, _offer) = connect(&backend).await;

    send_control(
        &client,
        ControlMessage::CaptureStart {
            video: false,
            audio: true,
        },
    )
    .await;

    let mut encoder = OpusEncoder::new(EncoderConfig::opus()).expect("Opus encoder");
    let tone = AudioFrame::silent(0, 0, 48_000, 2, 960).expect("silent tone");
    let packet = encoder.encode(tone).await.expect("Opus encode");
    assert!(!packet.data.is_empty());

    let mic_channel = channel(&client, ChannelKind::Mic);
    client
        .send(mic_channel, Bytes::from(packet.data.clone()))
        .await
        .unwrap();

    let hub = backend.client_capture(&session.id);
    let report = wait_for_report(&hub, |report| report.audio.samples >= 1).await;
    assert!(report.audio.started);
    assert_eq!(report.audio.packets, 1);
    assert_eq!(report.audio.bytes, packet.data.len() as u64);
    assert_eq!(report.audio.decode_errors, 0);
    // 20 ms of stereo audio is 960 frames per channel, 1920 interleaved
    // samples; allow exactly the decoded packet length.
    assert_eq!(report.audio.samples, 1_920);
    assert!(hub.latest_audio().is_some());

    client.close().await.unwrap();
}

async fn next_video_frame(
    client: &mut QuicTransport,
    assembler: &mut VideoAssembler,
) -> ozma_client_core::VideoFrame {
    timeout(STEP_TIMEOUT, async {
        loop {
            match client.recv().await {
                TransportEvent::Datagram { channel, payload } if channel == ChannelId::VIDEO => {
                    if let Some(frame) = assembler.push(&payload).expect("valid video fragment") {
                        return frame;
                    }
                }
                TransportEvent::Payload { .. } => continue,
                TransportEvent::Closed { reason } => {
                    panic!("transport closed while waiting for video: {reason:?}")
                }
                TransportEvent::Error { error } => panic!("transport error: {error}"),
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for a video frame")
}

#[tokio::test]
#[ignore = "timing-sensitive on shared CI; keyframe routing is covered deterministically by ozma-media and ozma-encode tests"]
async fn keyframe_request_forces_the_next_encoded_frame_to_be_an_idr() {
    let backend = backend();
    let (mut client, _session, _offer) = connect(&backend).await;

    let mut assembler = VideoAssembler::default();
    // The fake capture pipeline starts with an IDR and then sends P-frames.
    loop {
        let frame = next_video_frame(&mut client, &mut assembler).await;
        if !frame.keyframe {
            break;
        }
    }

    send_control(
        &client,
        ControlMessage::KeyframeRequest {
            track: Some(TrackId::new("video0")),
        },
    )
    .await;

    // Drop any access unit that was already in flight, then require an IDR
    // within a handful of frames. The software encoder's periodic GOP is 30
    // frames, so a keyframe this soon can only come from the request.
    assembler.reset();
    let mut frames_after_request = 0;
    let forced = loop {
        let frame = next_video_frame(&mut client, &mut assembler).await;
        frames_after_request += 1;
        if frame.keyframe {
            break frame;
        }
        assert!(
            frames_after_request < 12,
            "no keyframe within 12 frames of the request"
        );
    };
    assert!(forced.keyframe);
    assert!(frames_after_request <= 12);

    client.close().await.unwrap();
}

#[tokio::test]
async fn malformed_and_oversized_camera_input_is_counted_not_fatal() {
    let backend = backend();
    let (mut client, session, _offer) = connect(&backend).await;
    send_control(
        &client,
        ControlMessage::CaptureStart {
            video: true,
            audio: false,
        },
    )
    .await;

    let camera_channel = channel(&client, ChannelKind::Camera);
    // Not a video fragment header: the reassembler must reject it and count a
    // drop rather than panic or close the session.
    client
        .send(camera_channel, Bytes::from(vec![0xffu8; 32]))
        .await
        .unwrap();

    let hub = backend.client_capture(&session.id);
    timeout(STEP_TIMEOUT, async {
        loop {
            if hub.rejected() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("malformed fragment should be counted");

    // A session survives because the control loop is still responsive.
    send_control(&client, ControlMessage::Ping { nonce: 11 }).await;
    let pong = next_control(&mut client).await;
    assert_eq!(pong, ControlMessage::Pong { nonce: 11 });

    // An oversized reassembled unit is a typed error through the public hub.
    let error = hub
        .push_video(2, true, Bytes::from(vec![0u8; MAX_CAMERA_ACCESS_UNIT + 1]))
        .unwrap_err();
    assert_eq!(error.code, CLIENT_CAPTURE_REJECTED);
    assert!(hub.rejected() >= 2);

    client.close().await.unwrap();
}
