// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Native client camera and microphone uplink against a real node session.
//!
//! The test connects a [`NativeSessionClient`] to the node's real QUIC
//! listener, then starts capture with injected deterministic media: a fake
//! camera source encoded by the real software H.264 encoder, and a fake audio
//! tone encoded by real libopus. The node decodes exactly what it would decode
//! from a browser, and the HTTP capture endpoints are the verification surface.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use ozma_api::{ClientCaptureSink, NodeBackend};
use ozma_auth::{ConsentPolicy, MachineClass};
use ozma_client::{ClientCaptureOptions, ClientEvent, NativeClientConfig, NativeSessionClient};
use ozma_encode::H264SoftwareEncoder;
use ozma_media::{
    CaptureSource, EncodedFrame, Encoder, FakeCapture, Frame, FrameFormat, MediaError, MediaFormat,
};
use ozma_node::backend::SessionBackend;
use ozma_node::transport::QuicRuntime;
use ozma_protocol::{
    AudioFormat, AudioSpec, ClientId, Codec, ControlMessage, NodeCapabilities, Permission,
    PermissionSet, SessionSpec, TrackId, TrackKind, TrackSpec,
};
use tokio::time::timeout;
use tower::ServiceExt;

const STEP_TIMEOUT: Duration = Duration::from_secs(20);
const VIDEO_WIDTH: u32 = 64;
const VIDEO_HEIGHT: u32 = 48;

fn backend() -> Arc<SessionBackend> {
    Arc::new(SessionBackend::new(
        NodeCapabilities::fake("native-capture-test"),
        ConsentPolicy::for_machine(MachineClass::Server),
    ))
}

fn session_spec() -> SessionSpec {
    SessionSpec {
        client_id: ClientId::new("native-capture"),
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
        ttl_seconds: 120,
    }
}

async fn connect(
    backend: &Arc<SessionBackend>,
) -> (
    NativeSessionClient,
    ozma_protocol::Session,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<Mutex<Recording>>,
) {
    let runtime = QuicRuntime::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
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

    let mut config = NativeClientConfig::new();
    config.hello_timeout = STEP_TIMEOUT;
    let client = NativeSessionClient::connect_with_config(&offer, &certificate, config)
        .await
        .expect("native client connect");

    let camera_stopped = Arc::new(AtomicBool::new(false));
    let audio_stopped = Arc::new(AtomicBool::new(false));
    let recording = Arc::new(Mutex::new(Recording::default()));
    (client, session, camera_stopped, audio_stopped, recording)
}

/// What the recording encoder saw. `forced` marks an encode that followed a
/// keyframe request, so the test can attribute the IDR precisely.
#[derive(Default)]
struct Recording {
    frames: Vec<RecordedFrame>,
    keyframe_requests: u64,
}

struct RecordedFrame {
    keyframe: bool,
    forced: bool,
}

struct RecordingEncoder {
    inner: H264SoftwareEncoder,
    recording: Arc<Mutex<Recording>>,
    pending_request: bool,
}

impl RecordingEncoder {
    fn new(format: FrameFormat, recording: Arc<Mutex<Recording>>) -> Self {
        let request = MediaFormat::new(Codec::H264, format.width, format.height, 30, 1_000);
        Self {
            inner: H264SoftwareEncoder::new(format, request).expect("software H.264 encoder"),
            recording,
            pending_request: false,
        }
    }
}

#[async_trait]
impl Encoder for RecordingEncoder {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn input_format(&self) -> FrameFormat {
        self.inner.input_format()
    }

    fn output_format(&self) -> MediaFormat {
        self.inner.output_format()
    }

    async fn encode(&mut self, frame: Frame) -> Result<EncodedFrame, MediaError> {
        let forced = std::mem::take(&mut self.pending_request);
        let encoded = self.inner.encode(frame).await?;
        self.recording
            .lock()
            .expect("recording lock")
            .frames
            .push(RecordedFrame {
                keyframe: encoded.is_keyframe(),
                forced,
            });
        Ok(encoded)
    }

    async fn request_keyframe(&mut self) -> Result<(), MediaError> {
        self.pending_request = true;
        self.recording
            .lock()
            .expect("recording lock")
            .keyframe_requests += 1;
        self.inner.request_keyframe().await
    }
}

/// A source that records whether it was stopped, proving the worker exited.
struct FlaggedVideo {
    inner: FakeCapture,
    stopped: Arc<AtomicBool>,
}

#[async_trait]
impl CaptureSource for FlaggedVideo {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn format(&self) -> FrameFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        self.inner.next_frame().await
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped.store(true, Ordering::SeqCst);
        self.inner.stop().await
    }
}

impl Drop for FlaggedVideo {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// A paced, flagged fake microphone. The pacing keeps the single-threaded test
/// runtime free; the source itself is a real 440 Hz tone.
struct FlaggedAudio {
    inner: ozma_audio::FakeAudioSource,
    stopped: Arc<AtomicBool>,
}

#[async_trait]
impl ozma_audio::AudioSource for FlaggedAudio {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn format(&self) -> ozma_audio::AudioFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Result<ozma_audio::AudioFrame, ozma_audio::AudioError> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.inner.next_frame().await
    }

    async fn stop(&mut self) -> Result<(), ozma_audio::AudioError> {
        self.stopped.store(true, Ordering::SeqCst);
        self.inner.stop().await
    }
}

impl Drop for FlaggedAudio {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

async fn wait_for_capture(
    hub: &ozma_node::client_capture::ClientCaptureHub,
    predicate: impl Fn(&ozma_api::CaptureReport) -> bool,
) -> ozma_api::CaptureReport {
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
    .expect("timed out waiting for the node capture report")
}

#[tokio::test]
async fn native_client_camera_and_microphone_reach_the_node() {
    let backend = backend();
    let (mut client, session, camera_stopped, audio_stopped, recording) = connect(&backend).await;

    // The offer must carry the microphone channel because audio.mic = true.
    assert!(client
        .channel_map()
        .channel_for_kind(ozma_protocol::ChannelKind::Mic)
        .is_some());
    assert!(client
        .channel_map()
        .channel_for_kind(ozma_protocol::ChannelKind::Camera)
        .is_some());

    let format = FrameFormat::rgb24(VIDEO_WIDTH, VIDEO_HEIGHT);
    let video = FlaggedVideo {
        inner: FakeCapture::new(format).with_id("fake-camera").with_fps(60),
        stopped: Arc::clone(&camera_stopped),
    };
    let audio = FlaggedAudio {
        inner: ozma_audio::FakeAudioSource::new().unlimited(),
        stopped: Arc::clone(&audio_stopped),
    };
    let encoder = RecordingEncoder::new(format, Arc::clone(&recording));
    let audio_encoder =
        ozma_audio::OpusEncoder::new(ozma_audio::EncoderConfig::opus()).expect("Opus encoder");

    client
        .start_capture_with_media(
            ClientCaptureOptions {
                camera: Some("fake-camera".into()),
                mic: Some("default".into()),
                width: VIDEO_WIDTH,
                height: VIDEO_HEIGHT,
                fps: 30,
                bitrate_kbps: 1_000,
            },
            Some((Box::new(video), Box::new(encoder))),
            Some((Box::new(audio), Box::new(audio_encoder))),
        )
        .await
        .expect("start native capture");

    let state = client.capture_state().expect("capture state");
    assert!(state.video);
    assert!(state.audio);
    assert_eq!(state.video_codec.as_deref(), Some("h264"));
    assert_eq!(state.width, Some(VIDEO_WIDTH as u16));
    assert_eq!(state.height, Some(VIDEO_HEIGHT as u16));

    let hub = backend.client_capture(&session.id);
    let report = wait_for_capture(&hub, |report| {
        report.video.frames >= 3 && report.audio.samples > 0
    })
    .await;
    assert!(report.video.started);
    assert!(report.audio.started);
    assert_eq!(report.video.width, VIDEO_WIDTH);
    assert_eq!(report.video.height, VIDEO_HEIGHT);
    assert_eq!(report.video.codec, "h264");
    assert_eq!(report.video.decode_errors, 0);
    assert_eq!(report.audio.decode_errors, 0);
    assert!(report.audio.samples > 0);
    assert!(report.audio.packets > 0);

    // HTTP verification surface: counters and a real PPM picture.
    let dynamic: Arc<dyn NodeBackend> = backend.clone();
    let sink: Arc<dyn ClientCaptureSink> = backend.clone();
    let app = ozma_api::router::router_with_sinks(dynamic, None, Some(sink), None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sessions/{}/capture", session.id))
                .header("host", "127.0.0.1:3000")
                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["video"]["started"], true);
    assert!(json["video"]["frames"].as_u64().unwrap() >= 3);
    assert_eq!(json["video"]["width"], VIDEO_WIDTH);
    assert_eq!(json["video"]["height"], VIDEO_HEIGHT);
    assert_eq!(json["video"]["codec"], "h264");
    assert!(json["audio"]["samples"].as_u64().unwrap() > 0);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sessions/{}/capture/frame.ppm", session.id))
                .header("host", "127.0.0.1:3000")
                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
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
    let ppm_header = format!("P6\n{VIDEO_WIDTH} {VIDEO_HEIGHT}\n255\n");
    assert!(body.starts_with(ppm_header.as_bytes()));
    assert_eq!(
        body.len(),
        ppm_header.len() + (VIDEO_WIDTH * VIDEO_HEIGHT * 3) as usize
    );

    // A node-side KeyframeRequest must force the next camera encode to an IDR.
    timeout(STEP_TIMEOUT, async {
        loop {
            if recording.lock().expect("recording lock").frames.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("encoder did not produce enough frames");

    let payload = serde_json::to_vec(&ControlMessage::KeyframeRequest { track: None }).unwrap();
    let event = client
        .handle_control_for_test(&payload)
        .expect("keyframe control");
    match event {
        ClientEvent::UnknownControl { kind } => assert_eq!(kind, "keyframe_request"),
        other => panic!("expected UnknownControl for a keyframe request, got {other:?}"),
    }

    let forced = timeout(STEP_TIMEOUT, async {
        loop {
            let recording = recording.lock().expect("recording lock");
            if let Some(index) = recording.frames.iter().position(|frame| frame.forced) {
                let frame = &recording.frames[index];
                return (index, frame.keyframe);
            }
            drop(recording);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the encoder never honoured the keyframe request");
    assert!(
        forced.1,
        "the encode after a keyframe request must be an IDR"
    );
    assert!(
        recording.lock().expect("recording lock").keyframe_requests >= 1,
        "the runtime must call Encoder::request_keyframe"
    );

    // Stop: both workers terminate, counters freeze, and the node sees
    // CaptureStop.
    let before_stop = client.capture_state().expect("capture state");
    assert!(before_stop.frames_sent > 0);
    assert!(before_stop.mic_packets > 0);
    client.stop_capture().await.expect("stop capture");
    let after_stop = client.capture_state().expect("capture state after stop");
    assert_eq!(after_stop.video_codec.as_deref(), Some("h264"));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let later = client
        .capture_state()
        .expect("capture state stays available");
    assert_eq!(later.frames_sent, after_stop.frames_sent);
    assert_eq!(later.mic_packets, after_stop.mic_packets);
    assert!(
        camera_stopped.load(Ordering::SeqCst),
        "the camera source must be stopped"
    );
    assert!(
        audio_stopped.load(Ordering::SeqCst),
        "the microphone source must be stopped"
    );

    let stopped = wait_for_capture(&hub, |report| {
        !report.video.started && !report.audio.started
    })
    .await;
    assert!(!stopped.video.started);
    assert!(!stopped.audio.started);

    // stop_capture is idempotent.
    client
        .stop_capture()
        .await
        .expect("stopping twice is harmless");

    client.close().await.expect("close session");
}
