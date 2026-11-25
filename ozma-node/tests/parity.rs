// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Phase 0 parity harness: registration, HID v1, HTTP compat surface.
//!
//! This test target is deterministic and CI-safe. It needs no HID gadget,
//! hypervisor, QMP socket, Python process, or privileged port. The checked-in
//! fixtures under `tests/parity/` are the current Python/controller baseline;
//! see `tests/parity/README.md` for their source and the opt-in live Python
//! comparison that remains future work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use ozma_capture::{CaptureRegistry, FakeCaptureProvider};
use ozma_display::vm::VmConsoleBackend;
use ozma_input::{FakeInputSink, InputSink, RecordingSink, SharedSink};
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_node::api;
use ozma_node::controller_compat::{
    audio_router, codec_router, compat_router, connection_router, public_router, CodecControl,
    CompatStats, ConnectionInfo, ConnectionState, HidPacketDecoder, HidUdpListener,
    SINK_FAKE_RECORDING,
};
use ozma_node::display_compat::SnapshotControl;
use ozma_node::display_outputs::outputs_from_specs;
use ozma_node::hls::{
    ffmpeg_argv, stream_router, FfmpegSpawner, FrameSink, HlsConfig, HlsController, HlsEncoder,
    HlsSourceFactory, HlsStatus, SpawnedFfmpeg,
};
use ozma_node::webrtc::{
    HelperSpawner, Signaler, SpawnedHelper, WebRtcConfig, WebRtcControl, WebRtcError, WebRtcHelper,
};
use ozma_node::{platform, registration};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use ozma_protocol::{Codec, MediaFormat};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tower::ServiceExt;

const NODE_ID: &str = "parity-node._ozma._udp.local.";
const HOST: &str = "192.0.2.10";
const HID_PORT: u16 = 7331;
const HTTP_PORT: u16 = 7382;
const QUIC_PORT: u16 = 7384;
const DISPLAY_OUTPUTS_JSON: &str = r#"[{"name":"HDMI-1","width":1920,"height":1080}]"#;
const PCI_DEVICES_JSON: &str = r#"["0000:01:00.0"]"#;

// ---------------------------------------------------------------------------
// Registration parity
// ---------------------------------------------------------------------------

/// Mirror of the private `build_txt` in `main.rs` for a representative
/// hardware node (caps `hid,video,audio`, RTSP disabled). Keeping this in the
/// test is deliberate: the fixture is the Python baseline, not a snapshot of
/// the production helper.
fn hardware_txt() -> HashMap<String, String> {
    let mut txt = HashMap::new();
    txt.insert("proto".into(), "1".into());
    txt.insert("role".into(), "compute".into());
    txt.insert("hw".into(), "x86_64".into());
    txt.insert("fw".into(), "0.1.0".into());
    txt.insert("cap".into(), "hid,video,audio".into());
    txt.insert("api_port".into(), HTTP_PORT.to_string());
    txt.insert("quic_port".into(), QUIC_PORT.to_string());
    txt.insert("machine_class".into(), "server".into());
    txt.insert("capture_device".into(), "/dev/video0".into());
    txt.insert("display_outputs".into(), DISPLAY_OUTPUTS_JSON.into());
    txt.insert("pci_devices".into(), PCI_DEVICES_JSON.into());
    txt.insert("vm_guest_ip".into(), "10.0.0.42".into());
    txt.insert("stream_port".into(), HTTP_PORT.to_string());
    txt.insert("stream_path".into(), "/stream/stream.m3u8".into());
    txt.insert("audio_type".into(), "pipewire".into());
    txt.insert("audio_sink".into(), "ozma-node".into());
    // mDNS TXT also carries the resolved address; registration_payload must
    // still prefer the explicit host argument.
    txt.insert("host".into(), HOST.into());
    txt
}

fn baseline(path: &str) -> Value {
    serde_json::from_str(path).expect("baseline fixture is valid JSON")
}

/// Drop fixture metadata keys (`_baseline`, `_future_work`) before comparing.
fn without_metadata(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| !key.starts_with('_'));
    }
    value
}

fn field<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload
        .get(name)
        .unwrap_or_else(|| panic!("registration payload is missing {name:?}"))
}

#[test]
fn registration_payload_matches_python_controller_baseline() {
    let payload = registration::registration_payload(NODE_ID, HOST, HID_PORT, &hardware_txt());
    let expected = without_metadata(baseline(include_str!("parity/registration_baseline.json")));

    // Field-by-field against the checked-in Python/controller shape.
    assert_eq!(
        payload, expected,
        "registration payload drifted from the Python controller baseline"
    );

    // Required fields exist and keep their controller types.
    assert!(field(&payload, "id").is_string());
    assert!(field(&payload, "host").is_string());
    assert!(field(&payload, "port").is_u64());
    assert_eq!(payload["port"], u64::from(HID_PORT));

    // Every TXT-derived, controller-facing value is a string. The controller
    // parses ports with int(...) and JSON lists with json.loads(...), and the
    // DirectRegisterRequest model declares all of these as `str`.
    for name in [
        "proto",
        "role",
        "hw",
        "fw",
        "cap",
        "api_port",
        "quic_port",
        "machine_class",
        "capture_device",
        "display_outputs",
        "pci_devices",
        "vm_guest_ip",
        "stream_port",
        "stream_path",
        "audio_type",
        "audio_sink",
    ] {
        assert!(
            field(&payload, name).is_string(),
            "controller-facing field {name} must be a string, got {}",
            field(&payload, name)
        );
    }

    // Registration field behavior the controller depends on.
    assert_eq!(payload["api_port"], HTTP_PORT.to_string());
    assert_eq!(payload["quic_port"], QUIC_PORT.to_string());
    assert_eq!(payload["stream_port"], HTTP_PORT.to_string());
    assert_eq!(payload["stream_path"], "/stream/stream.m3u8");
    assert_eq!(payload["machine_class"], "server");
    assert_eq!(payload["capture_device"], "/dev/video0");
    assert_eq!(payload["audio_type"], "pipewire");
    assert_eq!(payload["audio_sink"], "ozma-node");
    assert_eq!(payload["vm_guest_ip"], "10.0.0.42");

    // JSON-encoded lists stay JSON so controller/api.py can json.loads them.
    let displays: Value =
        serde_json::from_str(payload["display_outputs"].as_str().unwrap()).unwrap();
    assert_eq!(displays[0]["name"], "HDMI-1");
    assert_eq!(displays[0]["width"], 1920);
    assert_eq!(displays[0]["height"], 1080);
    let pci: Value = serde_json::from_str(payload["pci_devices"].as_str().unwrap()).unwrap();
    assert_eq!(pci, json!(["0000:01:00.0"]));
}

#[test]
fn registration_txt_cannot_overwrite_the_resolved_host() {
    let mut txt = hardware_txt();
    txt.insert("host".into(), "txt-host-should-lose".into());

    let payload = registration::registration_payload(NODE_ID, HOST, HID_PORT, &txt);

    assert_eq!(payload["host"], HOST);
    assert_ne!(payload["host"], "txt-host-should-lose");
}

#[test]
fn minimal_registration_keeps_the_required_controller_fields() {
    let payload = registration::registration_payload(NODE_ID, HOST, HID_PORT, &HashMap::new());

    assert_eq!(
        payload,
        json!({
            "id": NODE_ID,
            "host": HOST,
            "port": HID_PORT,
        })
    );
}

#[test]
fn multi_display_registration_carries_validated_console_outputs() {
    let specs = vec![
        "0:Console_0:1920x1080:primary:dbus".to_owned(),
        "1:Console_1:1280x1024:dbus".to_owned(),
    ];
    let selection = outputs_from_specs(&specs, VmConsoleBackend::Dbus, Some("dbus0"), Some(1))
        .expect("valid multi-console topology");
    assert_eq!(selection.active_console, 1);

    let mut txt = hardware_txt();
    txt.insert("display_outputs".into(), selection.to_json());
    let payload = registration::registration_payload(NODE_ID, HOST, HID_PORT, &txt);

    let displays: Value =
        serde_json::from_str(payload["display_outputs"].as_str().unwrap()).unwrap();
    assert_eq!(displays.as_array().unwrap().len(), 2);
    assert_eq!(displays[0]["index"], 0);
    assert_eq!(displays[0]["source_type"], "dbus");
    assert_eq!(displays[0]["capture_source_id"], "");
    assert_eq!(displays[0]["primary"], true);
    assert_eq!(displays[1]["index"], 1);
    assert_eq!(displays[1]["capture_source_id"], "dbus0");
    assert_eq!(displays[1]["width"], 1280);
    assert_eq!(displays[1]["height"], 1024);
    assert_eq!(displays[1]["primary"], false);
}

#[derive(Clone, Default)]
struct Captured {
    registrations: Arc<Mutex<Vec<Value>>>,
    heartbeats: Arc<Mutex<Vec<Value>>>,
}

impl Captured {
    fn registrations(&self) -> Vec<Value> {
        self.registrations.lock().unwrap().clone()
    }

    fn heartbeats(&self) -> Vec<Value> {
        self.heartbeats.lock().unwrap().clone()
    }
}

async fn capture_registration(
    State(captured): State<Captured>,
    Json(body): Json<Value>,
) -> Json<Value> {
    captured.registrations.lock().unwrap().push(body);
    Json(json!({ "ok": true }))
}

async fn capture_heartbeat(
    State(captured): State<Captured>,
    Json(body): Json<Value>,
) -> Json<Value> {
    captured.heartbeats.lock().unwrap().push(body);
    Json(json!({ "ok": true }))
}

/// In-process fake controller on an ephemeral loopback port.
async fn spawn_fake_controller() -> (String, Captured, tokio::task::JoinHandle<()>) {
    let captured = Captured::default();
    let app = Router::new()
        .route("/api/v1/nodes/register", post(capture_registration))
        .route("/api/v1/nodes/heartbeat", post(capture_heartbeat))
        .with_state(captured.clone());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake controller on an ephemeral port");
    let address = listener.local_addr().expect("fake controller address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), captured, task)
}

async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let started = tokio::time::Instant::now();
    while !predicate() {
        assert!(
            started.elapsed() < timeout,
            "timed out after {timeout:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn fake_controller_receives_registration_and_heartbeat() {
    let (base_url, captured, server) = spawn_fake_controller().await;
    let txt = hardware_txt();

    registration::register(&base_url, NODE_ID, HOST, HID_PORT, &txt)
        .await
        .expect("registration is accepted by the fake controller");

    let registrations = captured.registrations();
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0],
        without_metadata(baseline(include_str!("parity/registration_baseline.json"))),
        "the JSON the fake controller captured differs from the baseline fixture"
    );

    let heartbeat = tokio::spawn({
        let base_url = base_url.clone();
        async move { registration::heartbeat_loop(&base_url, NODE_ID, 1).await }
    });

    wait_until(
        || !captured.heartbeats().is_empty(),
        Duration::from_secs(5),
        "the fake controller to receive a heartbeat",
    )
    .await;

    heartbeat.abort();
    let _ = heartbeat.await;

    let heartbeats = captured.heartbeats();
    assert_eq!(heartbeats[0], json!({ "node_id": NODE_ID }));
    assert!(
        heartbeats[0]["node_id"].is_string(),
        "heartbeat node_id must stay a string"
    );

    server.abort();
    let _ = server.await;
}

// ---------------------------------------------------------------------------
// HID v1 parity
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HidBaseline {
    cases: Vec<HidCase>,
}

#[derive(Deserialize)]
struct HidCase {
    name: String,
    steps: Vec<HidStep>,
}

#[derive(Deserialize)]
struct HidStep {
    packet: String,
    events: Vec<InputEvent>,
}

fn parse_hex(packet: &str) -> Vec<u8> {
    packet
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).expect("fixture packet is hex"))
        .collect()
}

fn key(code: u32, down: bool) -> InputEvent {
    InputEvent::Key {
        code,
        down,
        modifiers: KeyModifiers(0),
    }
}

fn absolute(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerAbsolute {
        display_id: None,
        x,
        y,
    }
}

#[test]
fn hid_v1_decoder_matches_python_translation_baseline() {
    let baseline: HidBaseline = serde_json::from_str(include_str!("parity/hid_v1_baseline.json"))
        .expect("HID baseline fixture is valid JSON");

    assert!(!baseline.cases.is_empty());
    for case in baseline.cases {
        let name = case.name;
        let mut decoder = HidPacketDecoder::new();
        for (index, step) in case.steps.into_iter().enumerate() {
            let packet = parse_hex(&step.packet);
            let events = decoder
                .decode(&packet)
                .unwrap_or_else(|error| panic!("{name} step {index} failed to decode: {error}"));
            assert_eq!(events, step.events, "{name} step {index}");
        }
    }
}

#[tokio::test]
async fn udp_hid_listener_records_and_injects_baseline_events() {
    let recording = Arc::new(RecordingSink::new(FakeInputSink::new()));
    let sink: SharedSink = recording.clone();
    let stats = CompatStats::default();
    let listener = HidUdpListener::bind(0, sink, stats.clone())
        .await
        .expect("bind UDP HID listener on an ephemeral port");
    let address = listener.local_addr().expect("listener address");
    assert_ne!(address.port(), 0);
    assert_eq!(stats.snapshot().bound_port, address.port());

    let task = tokio::spawn(async move { listener.run().await });
    let sender = tokio::net::UdpSocket::bind(("127.0.0.1", 0))
        .await
        .expect("bind UDP sender");

    // The default fake sink is absolute-pointer only, so this sequence keeps
    // relative motion out of the listener test (it is covered by the decoder
    // fixture) and all submitted events are accepted.
    let packets = [
        "01 00 00 04 00 00 00 00 00", // "a" down
        "01 00 00 00 00 00 00 00 00", // "a" up
        "02 01 40 01 F0 00 00",       // absolute (320,240), left button down
    ];
    for packet in packets {
        sender
            .send_to(&parse_hex(packet), address)
            .await
            .expect("send HID packet");
    }

    wait_until(
        || recording.events().len() >= 4,
        Duration::from_secs(2),
        "the listener to submit four input events",
    )
    .await;

    assert_eq!(
        recording.take_events(),
        vec![
            key(0x04, true),
            key(0x04, false),
            absolute(320, 240),
            InputEvent::Button {
                button: PointerButton::Left,
                down: true,
            },
        ]
    );

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.status, "running");
    assert_eq!(snapshot.packets_received, 3);
    assert_eq!(snapshot.packets_rejected, 0);
    assert_eq!(snapshot.events, 4);
    assert_eq!(snapshot.submit_errors, 0);
    assert!(snapshot.last_error.is_none());

    task.abort();
    let _ = task.await;
    assert_eq!(stats.snapshot().status, "stopped");
}

#[test]
fn release_on_disconnect_contract_clears_held_input() {
    let recording = Arc::new(RecordingSink::new(FakeInputSink::new()));
    let sink: SharedSink = recording.clone();

    sink.submit(&key(0x04, true)).expect("key press accepted");
    sink.submit(&InputEvent::Button {
        button: PointerButton::Left,
        down: true,
    })
    .expect("button press accepted");
    assert!(recording.held().has_key(0x04));
    assert!(recording.held().has_button(PointerButton::Left));

    // This is the contract a future session-disconnect handler must call. It
    // is asserted here so the parity matrix can point at an executable test,
    // not just at the InputSink documentation.
    sink.release_all().expect("release_all is infallible here");

    assert!(recording.held().is_empty());
    assert_eq!(recording.events().last(), Some(&InputEvent::ReleaseAll));
}

// ---------------------------------------------------------------------------
// HTTP compatibility surface
// ---------------------------------------------------------------------------

fn assert_shape(value: &Value, shape: &Value, context: &str) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{context} must be a JSON object, got {value}"));
    let shape = shape
        .as_object()
        .unwrap_or_else(|| panic!("{context} baseline shape must be an object"));

    for (name, expected) in shape {
        let field = object
            .get(name)
            .unwrap_or_else(|| panic!("{context} is missing key {name:?}"));
        let expected = expected
            .as_str()
            .unwrap_or_else(|| panic!("{context}.{name} shape must be a string"));
        let matches = match expected {
            "bool" => field.is_boolean(),
            "string" => field.is_string(),
            "number" => field.is_number(),
            "object" => field.is_object(),
            "array" => field.is_array(),
            "string-or-null" => field.is_string() || field.is_null(),
            "number-or-null" => field.is_number() || field.is_null(),
            "object-or-null" => field.is_object() || field.is_null(),
            other => panic!("{context}.{name} has unknown shape {other:?}"),
        };
        assert!(
            matches,
            "{context}.{name} must be {expected}, got {field} ({})",
            field_type(field)
        );
    }
}

fn field_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("router response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{uri} did not return JSON: {error}"));
    (status, json)
}

#[tokio::test]
async fn health_and_connection_surface_match_baseline() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));

    let app = api::router(platform::fake());
    let (status, health) = get_json(&app, "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&health, &baseline["health"], "health");
    assert_eq!(health["ok"], true);
    assert_eq!(health["node_id"], "test-node");
    assert_eq!(health["protocol"]["major"], 1);
    assert_eq!(health["sessions"], 0);

    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "hardware",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let compat = connection_router(state);
    let (status, connection) = get_json(&compat, "/api/v1/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&connection, &baseline["connection"], "connection");
    assert_shape(&connection["hid"], &baseline["hid"], "connection.hid");
    assert_shape(&connection["input"], &baseline["input"], "connection.input");
    assert_shape(&connection["codec"], &baseline["codec"], "connection.codec");

    assert_eq!(connection["ok"], true);
    assert_eq!(connection["node_id"], NODE_ID);
    assert_eq!(connection["profile"], "hardware");
    assert_eq!(connection["api_port"], HTTP_PORT);
    assert_eq!(connection["quic_port"], QUIC_PORT);
    assert_eq!(connection["hid_udp_port"], HID_PORT);
    assert_eq!(connection["sink_kind"], SINK_FAKE_RECORDING);
    assert_eq!(connection["hid"]["status"], "stopped");
    assert_eq!(connection["hid"]["bound_port"], 0);
    assert_eq!(connection["hid"]["packets_received"], 0);
    assert_eq!(connection["hid"]["packets_rejected"], 0);
    assert_eq!(connection["hid"]["events"], 0);
    assert_eq!(connection["hid"]["submit_errors"], 0);
    assert!(connection["hid"]["last_error"].is_null());

    // The connection state was built without a sink or capabilities, so input
    // and codec must be honest about having nothing configured.
    assert_eq!(connection["input"]["sink_kind"], SINK_FAKE_RECORDING);
    assert_eq!(connection["input"]["available"], false);
    assert_eq!(connection["input"]["injecting"], false);
    assert!(connection["codec"]["current_encoder"].is_null());
    assert!(connection["codec"]["override"].is_null());
    assert_eq!(connection["codec"]["available"], json!([]));
    assert!(connection["codec"]["receipt"].is_null());
    assert_eq!(connection["codec"]["pending_restart"], false);
    assert_eq!(
        connection["audio"],
        baseline["stream_states"]["audio_not_configured"]
    );
    assert_eq!(connection["audio"], "not-configured");
    assert!(connection["audio_error"].is_null());

    // HLS is no longer "not-implemented": when `--hls` is off the honest state
    // is "not-configured". WebRTC is still absent and must stay so.
    assert_eq!(
        connection["hls"],
        baseline["stream_states"]["hls_not_configured"]
    );
    assert_eq!(connection["hls"], "not-configured");
    assert!(connection["hls_error"].is_null());
    assert_eq!(
        connection["webrtc"],
        baseline["stream_states"]["webrtc_not_configured"]
    );
    assert_eq!(connection["webrtc"], "not-configured");
    assert!(connection["webrtc_error"].is_null());
    assert_eq!(connection["webrtc_audio_configured"], false);
    assert!(connection["webrtc_audio_source"].is_null());
}

#[tokio::test]
async fn display_snapshot_and_info_match_baseline() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_snapshot(Some(SnapshotControl::new(
        CaptureRegistry::new().with(FakeCaptureProvider::new()),
    )));
    let app = compat_router(state.clone()).merge(public_router(state));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/display/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/jpeg");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let jpeg = response.into_body().collect().await.unwrap().to_bytes();
    assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&jpeg));
    decoder.decode().expect("snapshot is a valid JPEG");
    let image = decoder.info().expect("JPEG info");
    assert_eq!((image.width, image.height), (320, 240));

    let (status, info) = get_json(&app, "/display/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&info, &baseline["display_info"], "display_info");
    assert_eq!(info["ok"], true);
    assert_eq!(info["source_id"], "fake0");
    assert_eq!(info["source_name"], "Fake capture");
    assert_eq!(info["width"], 320);
    assert_eq!(info["height"], 240);
    assert_eq!(info["pixel_format"], "rgb24");
    assert_eq!(info["snapshot_available"], true);
    assert_eq!(info["snapshot_encoding"], "jpeg");
    assert_eq!(info["dimensions_source"], "last-frame");
    assert!(info["last_error"].is_null());
}

#[tokio::test]
async fn agent_metrics_is_a_flat_controller_metrics_document() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "hardware",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let app = public_router(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agent/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/json",
        "/agent/metrics must be JSON, not Prometheus text"
    );
    let bytes = body_bytes(response).await;
    let document: Value = serde_json::from_slice(&bytes).expect("metrics JSON");

    assert_shape(&document, &baseline["agent_metrics"], "agent_metrics");
    assert_eq!(document["ok"], true);
    assert_eq!(document["source"], "ozma-node");
    assert_eq!(document["scope"], "host");
    assert_eq!(document["node_id"], NODE_ID);
    assert!(document["unavailable"].is_array());

    // On Linux the procfs-backed values must be present and plausible; the
    // document must never contain a bearer token or an auth header.
    #[cfg(target_os = "linux")]
    {
        assert_eq!(document["available"], true);
        let cpu = document["cpu_usage"].as_f64().expect("cpu_usage");
        assert!(
            (0.0..=100.0).contains(&cpu),
            "cpu_usage out of range: {cpu}"
        );
        assert!(document["ram_total"].as_u64().unwrap() > 0);
        assert!(document["ram_used"].as_u64().unwrap() <= document["ram_total"].as_u64().unwrap());
        assert!(document["disk_total"].as_u64().unwrap() > 0);
        assert!(
            document["disk_used"].as_u64().unwrap() <= document["disk_total"].as_u64().unwrap()
        );
        assert!(document["uptime"].as_u64().is_some());
    }

    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!text.contains("token"));
    assert!(!text.contains("Authorization"));
}

#[tokio::test]
async fn legacy_routes_report_typed_501s_and_minimal_hints() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "hardware",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let app = compat_router(state.clone()).merge(public_router(state));

    let (status, hints) = get_json(&app, "/ui/hints?level=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&hints, &baseline["ui_hints"], "ui_hints");
    assert_eq!(hints["ok"], true);
    assert_eq!(hints["available"], false);
    assert_eq!(hints["windows"], json!([]));
    assert!(hints["focused_window"].is_null());

    let (status, node_status) = get_json(&app, "/node/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node_status["ok"], true);
    assert_eq!(node_status["node_id"], NODE_ID);
    assert_eq!(node_status["api_port"], HTTP_PORT);

    for (uri, code) in [
        ("/usb", "usb.unsupported"),
        ("/current", "power.current_unsupported"),
        ("/rgb/state", "rgb.unsupported"),
        ("/serial/buffer?lines=50", "serial.unsupported"),
    ] {
        let (status, body) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{uri}");
        assert_shape(&body, &baseline["compat_error"], uri);
        assert_eq!(body["ok"], false, "{uri}");
        assert_eq!(body["code"], code, "{uri}");
    }

    let (status, body) = post_json(&app, "/session/init", json!({})).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_shape(&body, &baseline["compat_error"], "session/init");
    assert_eq!(body["code"], "session.init_unsupported");
}

#[tokio::test]
async fn connection_reports_a_failed_hls_pipeline_with_an_error() {
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_hls(Some(HlsStatus::failed("no capture source is available")));
    let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(connection["hls"], "failed");
    assert_eq!(connection["hls_error"], "no capture source is available");
    assert_eq!(connection["webrtc"], "not-configured");
}

#[tokio::test]
async fn codec_override_shape_matches_baseline_and_reports_pending_restart() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let control = CodecControl::new(
        ozma_encode::EncoderRegistry::from_capabilities([
            ozma_encode::H264SoftwareEncoder::capability(),
        ]),
        MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
    );
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_codec_control(Some(control));
    let app = codec_router(state.clone());

    let (status, before) = get_json(&app, "/codec").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&before, &baseline["codec"], "codec");
    assert_eq!(before["current_encoder"], "h264");
    assert_eq!(before["available"], json!(["h264"]));
    assert_eq!(before["pending_restart"], false);
    assert_eq!(before["applied"], false);

    let (status, applied) = post_json(
        &app,
        "/codec",
        json!({"codec": "h264", "bitrate": "8M", "hw_accel": "software"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&applied, &baseline["codec_apply"], "codec_apply");
    // The pipeline did not restart, so no live switch is claimed.
    assert_eq!(applied["applied"], false);
    assert_eq!(applied["pending_restart"], true);
    assert!(applied["encoder"].is_null());
    assert_eq!(applied["hls"], "not-configured");
    assert_eq!(applied["receipt"]["requested"]["bitrate_kbps"], 8_000);

    let (_, after) = get_json(&app, "/codec").await;
    assert_shape(&after, &baseline["codec"], "codec");
    assert_eq!(after["override"]["preference"]["codec"], "h264");
    assert_eq!(after["receipt"]["selected"]["backend"], "software");
    assert_eq!(after["pending_restart"], true);
    assert_eq!(after["applied"], false);

    let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&connection["codec"], &baseline["codec"], "connection.codec");
    assert_eq!(connection["codec"]["pending_restart"], true);
    assert_eq!(connection["audio"], "not-configured");
    assert!(connection["audio_error"].is_null());
}

#[tokio::test]
async fn codec_override_applies_to_hls_and_reports_applied() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let directory = std::env::temp_dir().join(format!(
        "ozma-parity-codec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).expect("create HLS directory");
    std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");

    let spawner = Arc::new(ParityHlsSpawner::default());
    let factory: HlsSourceFactory = Arc::new(|| {
        Ok(Box::new(PendingSource::new(FrameFormat::rgb24(320, 240))) as Box<dyn CaptureSource>)
    });
    let controller = HlsController::with_spawner(
        factory,
        HlsConfig {
            output_dir: directory.clone(),
            ..HlsConfig::default()
        },
        spawner.clone(),
    );
    controller.start().await.expect("start HLS");

    let control = CodecControl::new(
        ozma_encode::EncoderRegistry::from_capabilities([parity_software_capability(Codec::Hevc)]),
        MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
    )
    .with_hls(Some(controller.clone()));
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "hardware",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_hls_controller(Some(controller.clone()))
    .with_codec_control(Some(control));
    let app = codec_router(state.clone());

    let (status, applied) = post_json(
        &app,
        "/codec",
        json!({"codec": "hevc", "bitrate": "2M", "hw_accel": "software"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_shape(&applied, &baseline["codec_apply"], "codec_apply");
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["pending_restart"], false);
    assert_eq!(applied["encoder"], "libx265");
    assert_eq!(applied["hls"], "running");
    assert_eq!(applied["receipt"]["selected"]["codec"], "hevc");
    assert_eq!(controller.config().video_encoder, HlsEncoder::Hevc);

    let spawns = spawner.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), 2);
    assert!(spawns[1]
        .windows(2)
        .any(|window| window == ["-c:v", "libx265"]));
    assert!(spawns[1]
        .windows(2)
        .any(|window| window == ["-hls_flags", "delete_segments"]));

    let (status, after) = get_json(&app, "/codec").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&after, &baseline["codec"], "codec");
    assert_eq!(after["applied"], true);
    assert_eq!(after["pending_restart"], false);
    assert_eq!(after["current_encoder"], "libx265");
    assert_eq!(after["override"]["preference"]["codec"], "hevc");
    assert_eq!(after["receipt"]["selected"]["codec"], "hevc");

    let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_shape(&connection["codec"], &baseline["codec"], "connection.codec");
    assert_eq!(connection["codec"]["applied"], true);
    assert_eq!(connection["codec"]["pending_restart"], false);
    assert_eq!(connection["hls"], "running");

    controller.stop().await.expect("stop HLS");
    let _ = std::fs::remove_dir_all(directory);
}

struct ParityHlsSink;

#[async_trait]
impl FrameSink for ParityHlsSink {
    async fn write(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct ParityHlsSpawner {
    spawns: Mutex<Vec<Vec<String>>>,
}

impl FfmpegSpawner for ParityHlsSpawner {
    fn spawn(
        &self,
        config: &HlsConfig,
        width: u32,
        height: u32,
    ) -> Result<SpawnedFfmpeg, ozma_node::hls::HlsError> {
        self.spawns
            .lock()
            .unwrap()
            .push(ffmpeg_argv(config, width, height));
        Ok(SpawnedFfmpeg {
            sink: Box::new(ParityHlsSink),
            child: None,
        })
    }
}

fn parity_software_capability(codec: Codec) -> ozma_encode::EncodeCapability {
    ozma_encode::EncodeCapability {
        codec,
        backend: ozma_protocol::EncoderBackend::Software,
        hardware: false,
        max_width: 1920,
        max_height: 1080,
        max_fps: 30,
        hdr: false,
        profiles: Vec::new(),
    }
}

#[tokio::test]
async fn audio_routes_remain_typed_501_without_a_router() {
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let app = audio_router(state);

    for path in [
        "/audio/apply",
        "/audio/remove",
        "/audio/play",
        "/audio/stop",
    ] {
        let (status, body) = post_json(&app, path, json!({})).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{path}");
        assert_eq!(body["ok"], false, "{path}");
        assert_eq!(body["code"], "audio-routing-unsupported", "{path}");
        assert!(body["remediation"].is_string(), "{path}");
    }
}

#[tokio::test]
async fn public_audio_discovery_and_sweep_are_typed_501s() {
    let baseline = baseline(include_str!("parity/http_baseline.json"));
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let app = public_router(state);

    let (status, nodes) = get_json(&app, "/audio/nodes").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_shape(
        &nodes,
        &baseline["audio_nodes_unsupported"],
        "audio_nodes_unsupported",
    );
    assert_eq!(nodes["ok"], false);
    assert_eq!(nodes["available"], false);
    assert_eq!(nodes["code"], "audio-nodes-unsupported");
    assert_eq!(nodes["nodes"], json!([]));
    assert_eq!(nodes["links"], json!([]));

    let (status, sweep) = post_json(
        &app,
        "/audio/sweep",
        json!({"source": "mic", "sink": "speaker"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_shape(
        &sweep,
        &baseline["audio_sweep_unsupported"],
        "audio_sweep_unsupported",
    );
    assert_eq!(sweep["ok"], false);
    assert_eq!(sweep["available"], false);
    assert_eq!(sweep["code"], "audio-sweep-unsupported");
    assert!(sweep["profile"].is_null());
}

#[tokio::test]
async fn webrtc_routes_report_not_configured_rather_than_404() {
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "hardware",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    );
    let app = compat_router(state);

    let (status, body) = post_json(
        &app,
        "/webrtc/offer",
        json!({"sdp": "v=0 parity offer", "type": "offer"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "webrtc-not-configured");

    let (status, body) = post_json(&app, "/webrtc/bitrate", json!({"bitrate": 4_000_000})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "webrtc-not-configured");
}

#[tokio::test]
async fn webrtc_routes_report_a_failed_helper_without_claiming_running() {
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_webrtc(Some(WebRtcControl::failed(
        "WebRTC unavailable: helper program is missing",
    )));
    let app = compat_router(state);

    let (status, connection) = get_json(&app, "/api/v1/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(connection["webrtc"], "failed");
    assert_eq!(
        connection["webrtc_error"],
        "WebRTC unavailable: helper program is missing"
    );

    let (status, body) = post_json(
        &app,
        "/webrtc/offer",
        json!({"sdp": "v=0 parity offer", "type": "offer"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "webrtc-unavailable");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("helper program is missing"));
}

#[tokio::test]
async fn webrtc_offer_proxies_to_a_fake_unix_socket_helper() {
    let (client, server) = UnixStream::pair().expect("unix stream pair");
    let (client_reader, client_writer) = client.into_split();
    let (server_reader, mut server_writer) = server.into_split();
    let helper_task = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read offer");
        let request: Value = serde_json::from_str(line.trim()).expect("offer JSON");
        assert_eq!(request["type"], "offer");
        assert_eq!(request["sdp"], "v=0 parity offer");
        server_writer
            .write_all(
                b"{\"type\":\"answer\",\"sdp\":\"v=0 parity answer\",\"session\":\"sess-1\"}\n",
            )
            .await
            .expect("write answer");
        server_writer.flush().await.expect("flush answer");

        line.clear();
        reader.read_line(&mut line).await.expect("read candidate");
        let request: Value = serde_json::from_str(line.trim()).expect("candidate JSON");
        assert_eq!(request["type"], "add-ice-candidate");
        assert_eq!(request["session"], "sess-1");
        assert_eq!(
            request["candidate"],
            "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host"
        );
        assert_eq!(request["sdpMLineIndex"], 0);
        server_writer
            .write_all(b"{\"ok\":true}\n")
            .await
            .expect("write candidate ack");
        server_writer.flush().await.expect("flush candidate ack");
    });

    let spawner = Arc::new(FakeHelperSpawner::new(Box::new(PairSignaler {
        reader: BufReader::new(client_reader),
        writer: client_writer,
    })));
    let directory = temp_dir("webrtc-offer");
    let config = WebRtcConfig {
        helper_program: std::path::PathBuf::from("/fake/helper"),
        helper_script: None,
        socket_path: directory.join("helper.sock"),
        shm_path: directory.join("frame.bgra"),
        width: 2,
        height: 2,
        scale_to_fit: false,
        fps: 30,
        bitrate_kbps: 4_000,
        audio_source: None,
        audio_backend: Default::default(),
        stun_server: None,
        turn_server: None,
        turn_user: None,
        turn_password: None,
        startup_timeout: Duration::from_millis(50),
        offer_timeout: Duration::from_secs(1),
    };
    let helper = WebRtcHelper::start_with_spawner(
        Box::new(PendingSource::new(FrameFormat::rgb24(2, 2))),
        config,
        spawner,
    )
    .expect("fake helper starts");

    let control = WebRtcControl::from_helper(helper);
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_webrtc(Some(control.clone()));
    let app = compat_router(state);

    // Running is claimed only because the fake socket is connected.
    let (status, connection) = get_json(&app, "/api/v1/connection").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        connection["webrtc"],
        baseline(include_str!("parity/http_baseline.json"))["stream_states"]["webrtc_running"]
    );
    assert_eq!(connection["webrtc"], "running");
    assert!(connection["webrtc_error"].is_null());

    let (status, body) = post_json(
        &app,
        "/webrtc/offer",
        json!({"sdp": "v=0 parity offer", "type": "offer"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "answer");
    assert_eq!(body["sdp"], "v=0 parity answer");
    assert_eq!(body["session"], "sess-1");

    let (status, body) = post_json(
        &app,
        "/webrtc/candidate",
        json!({
            "session": "sess-1",
            "candidate": "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host",
            "sdpMLineIndex": 0
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    control.shutdown().await;
    helper_task.await.expect("fake helper task");
}

#[tokio::test]
async fn webrtc_bitrate_is_not_reported_as_success_without_an_ack() {
    let (client, server) = UnixStream::pair().expect("unix stream pair");
    let (client_reader, client_writer) = client.into_split();
    let (server_reader, _server_writer) = server.into_split();
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read bitrate");
        let request: Value = serde_json::from_str(line.trim()).expect("bitrate JSON");
        assert_eq!(request["type"], "bitrate");
        assert_eq!(request["bitrate"], 4_000_000);
        // The real worker sends no acknowledgement; keep the socket open so the
        // node enforces its own bounded timeout instead of reading EOF.
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let spawner = Arc::new(FakeHelperSpawner::new(Box::new(PairSignaler {
        reader: BufReader::new(client_reader),
        writer: client_writer,
    })));
    let directory = temp_dir("webrtc-bitrate");
    let config = WebRtcConfig {
        helper_program: std::path::PathBuf::from("/fake/helper"),
        helper_script: None,
        socket_path: directory.join("helper.sock"),
        shm_path: directory.join("frame.bgra"),
        width: 2,
        height: 2,
        scale_to_fit: false,
        fps: 30,
        bitrate_kbps: 4_000,
        audio_source: None,
        audio_backend: Default::default(),
        stun_server: None,
        turn_server: None,
        turn_user: None,
        turn_password: None,
        startup_timeout: Duration::from_millis(50),
        offer_timeout: Duration::from_millis(100),
    };
    let helper = WebRtcHelper::start_with_spawner(
        Box::new(PendingSource::new(FrameFormat::rgb24(2, 2))),
        config,
        spawner,
    )
    .expect("fake helper starts");
    let control = WebRtcControl::from_helper(helper);
    let state = ConnectionState::new(
        ConnectionInfo::new(
            NODE_ID,
            "vm",
            HTTP_PORT,
            QUIC_PORT,
            HID_PORT,
            SINK_FAKE_RECORDING,
        ),
        CompatStats::default(),
    )
    .with_webrtc(Some(control.clone()));
    let app = compat_router(state);

    let (status, body) = post_json(&app, "/webrtc/bitrate", json!({"bitrate": 4_000_000})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "webrtc-helper-timeout");
    control.shutdown().await;
}

struct PendingSource {
    format: FrameFormat,
}

impl PendingSource {
    fn new(format: FrameFormat) -> Self {
        Self { format }
    }
}

#[async_trait]
impl CaptureSource for PendingSource {
    fn id(&self) -> &str {
        "parity-pending"
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        Err(MediaError::EndOfStream)
    }
}

struct FakeHelperSpawner {
    spawned: Mutex<Option<SpawnedHelper>>,
}

impl FakeHelperSpawner {
    fn new(signaler: Box<dyn Signaler>) -> Self {
        Self {
            spawned: Mutex::new(Some(SpawnedHelper {
                child: None,
                pid: Some(4_242),
                signaler,
            })),
        }
    }
}

impl HelperSpawner for FakeHelperSpawner {
    fn spawn(&self, config: &WebRtcConfig) -> Result<SpawnedHelper, WebRtcError> {
        self.spawned
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| WebRtcError::Spawn {
                program: config.helper_program.clone(),
                message: "fake spawner has no helper left".to_owned(),
            })
    }
}

struct PairSignaler {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

#[async_trait]
impl Signaler for PairSignaler {
    async fn write_line(&mut self, line: &str) -> Result<(), WebRtcError> {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))?;
        self.writer
            .flush()
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))
    }

    async fn read_line(&mut self) -> Result<Option<String>, WebRtcError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|error| WebRtcError::Io(error.to_string()))?;
        if read == 0 {
            Ok(None)
        } else {
            Ok(Some(line))
        }
    }
}

async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("router response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{uri} did not return JSON: {error}"));
    (status, json)
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "ozma-parity-{label}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).expect("create temp directory");
    directory
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn stream_files_are_served_only_from_the_configured_directory() {
    let root = temp_dir("stream");
    let stream_dir = root.join("hls");
    std::fs::create_dir_all(&stream_dir).unwrap();
    std::fs::write(stream_dir.join("stream.m3u8"), "#EXTM3U\n").unwrap();
    std::fs::write(stream_dir.join("seg_00000.ts"), b"segment").unwrap();
    std::fs::write(root.join("secret.txt"), b"do not serve me").unwrap();

    // The controller proxies `/stream/{file}` without credentials, so the
    // route is merged into the unauthenticated media surface.
    let app = api::router(platform::fake()).merge(stream_router(stream_dir.clone()));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stream/stream.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"#EXTM3U\n");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stream/seg_00000.ts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"segment");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stream/missing.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a missing manifest must be a 404, never an empty success"
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/stream/../secret.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_client_error(),
        "path traversal must be rejected, got {}",
        response.status()
    );
    assert_ne!(body_bytes(response).await, b"do not serve me");

    let _ = std::fs::remove_dir_all(root);
}
