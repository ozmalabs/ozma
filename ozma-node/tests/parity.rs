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

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use ozma_input::{FakeInputSink, InputSink, RecordingSink, SharedSink};
use ozma_node::api;
use ozma_node::controller_compat::{
    connection_router, CompatStats, ConnectionInfo, ConnectionState, HidPacketDecoder,
    HidUdpListener, SINK_FAKE_RECORDING,
};
use ozma_node::hls::{stream_router, HlsStatus};
use ozma_node::{platform, registration};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use serde::Deserialize;
use serde_json::{json, Value};
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
    assert_eq!(connection["audio"], "not-implemented");

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
        baseline["stream_states"]["webrtc_not_implemented"]
    );
    assert_eq!(connection["webrtc"], "not-implemented");
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
    assert_eq!(connection["webrtc"], "not-implemented");
}

#[tokio::test]
async fn webrtc_routes_are_absent_rather_than_faked() {
    let app = api::router(platform::fake());

    for (method, uri) in [("POST", "/webrtc/offer"), ("GET", "/webrtc/bitrate")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("router response");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} must stay absent until a real WebRTC pipeline exists"
        );
    }
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
