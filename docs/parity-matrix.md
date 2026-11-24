# Parity matrix

Status of Python → Rust node parity. This matrix is the Gate 0 artifact from
[`docs/rust-reconciliation-plan.md`](rust-reconciliation-plan.md) §3 and is
also the acceptance tracker for
[`docs/node-hermes-parity.md`](node-hermes-parity.md) §15–§16.

**Rule:** no row is marked `green` from code inspection alone. A green row must
name an automated test (or command) that asserts the behavior in this checkout
and that passed. A fixture assertion is not a live Python comparison; rows that
only compare against the checked-in baseline stay at most `partial` unless the
row itself is the Rust behavior under test.

## Status legend

| Status | Meaning |
|---|---|
| `green` | A checked-in test asserts the behavior and the evidence command passed. |
| `partial` | Part of the behavior is asserted; a required part is missing, unwired, or only compared against a hand-written baseline. |
| `not-run` | The behavior may exist, but this Phase 0 run did not execute its gate (external environment, controller pytest, demo, clean host). |
| `not-implemented` | The Rust path does not exist yet. Tests may assert that the surface honestly reports absence. |

## Gate 0 summary

- Parity harness: `ozma-node/tests/parity.rs` plus fixtures in
  `ozma-node/tests/parity/`. It runs without a HID gadget, hypervisor, QMP
  socket, Python process, or privileged port.
- Current run (2026-11-24): `cargo test -p ozma-node --test parity` →
  **16 passed, 0 failed**; `cargo test -p ozma-node` → **173 passed,
  0 failed, 3 ignored**; `cargo test --workspace` → **1413 passed, 0 failed,
  16 ignored**; `cargo check --workspace --all-targets` succeeded.
- Fake QMP support: `ozma-qmp/tests/common/` provides a greeting/capabilities
  server and an SCM_RIGHTS `getfd` peer. `ozma-capture/tests/qmp_display.rs`
  and `ozma-power/tests/qmp_power.rs` reuse the same protocol shape for their
  provider tests. `ozma-qmp/tests/shared_client.rs` and
  `ozma-node/tests/shared_qmp.rs` add a server that rejects a second
  connection and prove one `SharedQmpClient` serves power, screendump, and
  input. No real QEMU is needed for the default suite.

## Matrix

| Area | Status | What is asserted today | Evidence | Gap / next gate |
|---|---|---|---|---|
| Registration — mDNS TXT shape | `partial` | `registration_payload` produces the controller's `DirectRegisterRequest` fields from the mDNS TXT map: required `id`/`host`/`port`, string-typed controller fields, ports and JSON lists, `machine_class`, `capture_device`, `display_outputs`, `pci_devices`, `audio_type`, `audio_sink`, `vm_guest_ip`, `stream_port`/`stream_path`. TXT `host` cannot replace the resolved host. | `cargo test -p ozma-node --test parity registration_` (3 tests) | No test browses/advertises real mDNS. `mdns::advertise` and controller-side discovery remain untested here. |
| Registration — direct POST + heartbeat | `green` | The real `registration::register` POSTs to an in-process axum fake controller; the captured JSON equals `tests/parity/registration_baseline.json`. The real `heartbeat_loop` sends `{"node_id": ...}` and the fake controller captures it. | `cargo test -p ozma-node --test parity fake_controller_receives_registration_and_heartbeat` | The peer is a fixture-shaped fake, not the live Python controller. Live controller POST is a later opt-in run. |
| HTTP compat surface — `/health`, `/api/v1/connection`, `/power/*` | `green` | Keys and JSON types match `tests/parity/http_baseline.json`; health reports `ok`, node id, protocol major, session count; connection reports ports, sink kind, HID stats, power summary, input sink availability/injection, codec state (`current_encoder`, `override`, `available`, `receipt`, `pending_restart`, `applied`), `hls` (`not-configured`/`running`/`failed`), `hls_error`, `webrtc` (`not-configured`/`running`/`failed`), `webrtc_error`, `webrtc_audio_configured`/`webrtc_audio_source` (configuration intent only), and `audio` (`not-configured`/`running`/`failed`) plus `audio_error`; `/power/state` and `/power/{action}` are bearer-guarded and covered with a fake backend. | `cargo test -p ozma-node --test parity health_and_connection_surface_match_baseline codec_override_shape_matches_baseline_and_reports_pending_restart codec_override_applies_to_hls_and_reports_applied audio_routes_remain_typed_501_without_a_router`, `cargo test -p ozma-node` (compat tests) | Unit/parity tests assert the WebRTC status with a fake helper; a live GStreamer helper separately reported `running`, with live audio configured but samples not claimed by this object (`docs/gate2-live-webrtc-report.md`). `/stream/*` auth behavior is asserted in the `ozma-node` API unit test, not yet against the live controller. |
| HTTP compat surface — display snapshot/info + legacy controller aliases | `partial` | `GET /display/snapshot` opens the node's capture registry source once, reads one frame, and returns a real `image/jpeg` (`Cache-Control: no-store`) encoded from RGB24/BGRA or passed-through MJPEG; missing/busy source, timeout, and unsupported layouts are typed 503/504 JSON with remediation. `GET /display/info` reports source id/name, width/height, pixel format, and snapshot availability without opening the device. `GET /node/status` aliases the connection summary, and `GET /ui/hints` returns the minimal empty hint shape (`windows`, `focused_window`, `controls`, `focused_control`, `available:false`). These four read-only routes, plus `GET /agent/metrics` and the typed audio discovery/sweep stubs, are mounted in `controller_compat::public_router` **after** the bearer layer (like `/stream/*`) because the unmodified controller polls them without a node token. `GET /display/mjpeg` stays guarded (typed 501), as do `/usb`, `/rgb/state`, `/rgb/effect`, `/current`, `/session/init`, and `/serial/*` (typed 501 JSON stubs, never success-shaped). | `cargo test -p ozma-node --lib display_compat:: controller_compat::tests::display_ controller_compat::tests::legacy_routes controller_compat::tests::ui_hints controller_compat::tests::node_status`, `cargo test -p ozma-node --test parity display_snapshot_and_info_match_baseline legacy_routes_report_typed_501s_and_minimal_hints`, `cargo test -p ozma-node api::tests::public_controller_routes_bypass_bearer_but_controls_stay_guarded`, `cargo test -p ozma-encode` | No live QMP/VNC/D-Bus/camera or controller/dashboard run in CI; tests inject a fake capture registry. Serial/RGB/USB/current and encrypted `/session/init` have no Rust implementation yet. |
| HTTP compat surface — `/agent/metrics` | `partial` | Returns `200 application/json` with a flat host metrics document for `controller/device_metrics.py` (which calls `update_batch` and therefore only records top-level numbers). Numeric keys are `cpu_usage` (percent), `ram_used`/`ram_total` (bytes), `disk_used`/`disk_total` (bytes), `net_rx_rate`/`net_tx_rate` (bytes/s, computed from `/proc/net/dev` deltas across two samples at least 50 ms apart), and `uptime` (seconds). Metadata carries `ok`, `available`, `source`, `scope`, `node_id`, an `unavailable` list, and `notes` (for example whether CPU is `since-boot` or `since-last-sample`). Reads are limited to `/proc/{stat,meminfo,net/dev,uptime}`, each capped at 256 KiB, plus one `statvfs`; there is no subprocess, network poll, or invented zero. Missing metrics are omitted and named, and `available:false` is returned when nothing could be read. | `cargo test -p ozma-node --lib agent_metrics::`, `cargo test -p ozma-node --test parity agent_metrics_is_a_flat_controller_metrics_document`, `cargo test -p ozma-node api::tests::public_controller_routes_bypass_bearer_but_controls_stay_guarded` | No live controller `device_metrics` poll and no non-Linux path in CI; CPU temperature, GPU, fan, and power-draw metrics are explicitly unavailable (no sensor source). Public route exposes host resource totals by design, but no tokens, paths, or environment values. |
| WebRTC audio CLI wiring | `partial` | `--webrtc-audio-source` (`OZMA_WEBRTC_AUDIO_SOURCE`) and `--webrtc-audio-backend auto|pulse|pipewire` (`OZMA_WEBRTC_AUDIO_BACKEND`) flow through `NodeConfig` into `WebRtcConfig`; empty/whitespace sources are rejected, audio is off unless a source is supplied, and the helper argv only gains `--audio-source`/`--audio-backend` when configured. `WebRtcAudioBackend` is a clap value enum with `pipewire` spelled explicitly. `/api/v1/connection` reports `webrtc_audio_configured` and `webrtc_audio_source` as configuration intent only; it never claims the worker opened the monitor or produced samples. The helper's Opus branch now uses `S16LE` (the only format `opusenc` accepts); a live GStreamer client decoded 1020 Opus buffers from a disposable null-sink monitor (`docs/gate2-live-webrtc-report.md`). | `cargo test -p ozma-node --lib main::tests::webrtc_audio config::tests::webrtc_audio webrtc::tests::argv_appends_audio controller_compat::tests::connection_reports_webrtc_audio`, `python3 softnode/gst_webrtc_worker_test.py`, live `python3 scripts/gate2/webrtc-client.py --audio` | No browser/controller audio run in CI; live audio used a synthetic tone on a disposable null sink, not guest audio or a real output device. Without `--webrtc-audio-source` the helper is video-only and an offered audio m-line is answered `inactive`. |
| HTTP compat surface — `/input/key`, `/input/mouse`, `/input/type` | `green` | Key requests take Python-compatible Linux evdev `keycode` + `down` and map through `ozma_input::linux_keycode_to_hid_usage` into `InputEvent::Key` on the one shared sink; unknown/out-of-range keycodes are typed 400s and a duplicate press is accepted without changing held state. Mouse requests take `x`/`y` in `0..=32767`, `action` (`move`/`press`/`release`/`click`), and button `0`/`1`→left, `2`→right, `4`→middle, `8`→back, `16`→forward; click emits press, a 20 ms hold, and release. Typing maps printable ASCII, newline, and tab to HID usages with an explicit left shift, rejects non-ASCII/oversized text (4096 chars), and sleeps 15 ms between characters. Responses include `"injected": false` for a recording-only sink; a missing sink is 503. | `cargo test -p ozma-node --lib controller_compat::tests::input_`, `cargo test -p ozma-node --lib controller_compat::tests::text_mapping_` | No real QMP/uinput/gadget injection in CI; text is US-layout ASCII only; click/typing delays are fixed bounds rather than guest-acknowledged pacing. |
| HTTP compat surface — `/codec` | `partial` | `GET /codec` reports `current_encoder`, `available` (unique video codecs from the real `EncoderRegistry`), the stored `override`, its policy `receipt`, `pending_restart`, and `applied`. `POST /codec` maps the controller's `CodecConfig.to_dict()` fields: `codec` (`h264`, `h265`/`hevc`, `av1`, `mjpeg`), `bitrate` (`8M`, `8000k`, `8Mbps`, numeric kbps), and `hw_accel` (`auto`, `nvenc`, `vaapi`, `qsv`, `v4l2m2m`, `software`). A valid request is evaluated with `EncoderRegistry::override_receipt`; when HLS is running, the controller stops the old `HlsPipeline` and starts a fresh capture source with the mapped encoder: software `h264`→`libx264`, `hevc`→`libx265`, `av1`→`libsvtav1`, `mjpeg`→`mjpeg`, or hardware `h264/hevc`→`{h264,hevc}_{vaapi,nvenc,qsv,v4l2m2m}`. VAAPI argv carries `-vaapi_device <path> -vf format=nv12,hwupload`; NVENC uses `-preset p4 -tune ll -rc cbr`; QSV uses `-preset veryfast`; V4L2M2M uses the bare encoder. Every path keeps `-hls_flags delete_segments`. Hardware is never assumed from its name: start/restart consults an injectable `EncoderProbe` (production runs bounded `ffmpeg -encoders` once; tests inject a static list) and a missing VAAPI device is a typed error with remediation. Only a successful restart returns `applied:true`, `pending_restart:false`, the actual `encoder`/`requested_encoder`, backend, VAAPI device (when relevant), and the `hls` status; an unavailable encoder is a typed 502 (`encoder.unavailable`) that restores the previous pipeline. A software substitution happens only with explicit `allow_software_fallback:true` and is reported as `fallback:true` plus `requested_encoder` and `fallback_reason`. With HLS off the override is stored and answered `applied:false, pending_restart:true` for a future start. Unknown codecs/bitrates/backends, unavailable hardware, and denied fallback are typed errors carrying the real capability list; no-video is a typed 503. WebRTC keeps its existing encoder: neither `applied` nor `current_encoder` claims a WebRTC switch. | `cargo test -p ozma-node --lib hls::tests::`, `cargo test -p ozma-node --lib controller_compat::tests::codec_route_`, `cargo test -p ozma-node --test parity codec_`, `cargo test -p ozma-encode` | Hardware argv, probe availability, VAAPI-device validation, explicit fallback, and `/codec` mapping are covered with an injected probe/spawner; no real ffmpeg or GPU runs in CI, and the live-playlist test is `#[ignore]`. WebRTC codec switching remains unimplemented. |
| Audio — `/audio/apply`, `/audio/remove`, `/audio/play`, `/audio/stop` compatibility | `partial` | `--audio-router none` (default) keeps typed 501 `audio-routing-unsupported`; `pipewire`/`pulseaudio` build a real `AudioRouter` over an existing graph (`pactl` + `pw-play`/`paplay`) and return typed 200/400/502/503 results. `/audio/apply` derives/requires a sink, rejects `source` with a 501 `audio-source-unsupported`, and validates volume `0..=100`; `/audio/remove` restores the named profile (an empty body falls back to the most recently applied profile, matching the Python controller's proxy) and `/audio/play` resolves a single track name under `--audio-track-root`; `/audio/stop` kills the player. No router call means no success. `/api/v1/connection.audio` is `running` for a healthy configured router, `not-configured` without one, and `failed` plus `audio_error` when the startup check (missing `pactl` or a failed/timed-out `pactl info`) fails. The control routes stay behind the bearer guard. `GET /audio/nodes` and `POST /audio/sweep` are public typed 501 stubs: `AudioRouter` has no graph-enumeration or room-correction operation, so `/audio/nodes` returns the soft-node shape with `nodes: []`, `links: []`, and `default_sink`/`default_source: null`, while `/audio/sweep` returns `ok:false`, `available:false`, and `profile:null`. Neither fabricates a graph or a sweep profile. | `cargo test -p ozma-node --lib controller_compat::tests::audio_`, `cargo test -p ozma-node --test parity audio_routes_remain_typed_501_without_a_router public_audio_discovery_and_sweep_are_typed_501s`, `cargo test -p ozma-audio` | No native DSP/room correction, no PipeWire node enumeration, no live PipeWire/PulseAudio server or `pactl`/`pw-play` run in CI; per-VM sink creation is an `ozma-softnode --provision-audio` concern. The fake router asserts argv-equivalent trait calls and state, not audible output. |
| Input — HID v1 keyboard | `green` | Keyboard press, release, multiple keys, duplicate report suppression, and modifier press/release produce the normalized `InputEvent` sequence in `tests/parity/hid_v1_baseline.json`. | `cargo test -p ozma-node --test parity hid_v1_decoder_matches_python_translation_baseline` | None for v1 decode. Python baseline maps HID→evdev in the sink; the fixture asserts the normalized adapter output. |
| Input — HID v1 pointer (absolute/relative/buttons/wheel) | `green` | Absolute and relative motion, button press/release, and wheel sign match the fixture. The real `HidUdpListener` on an ephemeral port decodes and submits events through `RecordingSink<FakeInputSink>`. | `cargo test -p ozma-node --test parity hid_v1_decoder_matches_python_translation_baseline udp_hid_listener_records_and_injects_baseline_events` | No real HID gadget or evdev injection in CI; that is the hardware opt-in path. |
| Input — release on disconnect | `partial` | The `InputSink::release_all` contract clears held keys/buttons and records `ReleaseAll` for the gadget, uinput, QMP, and recording sinks; this is the call a disconnect handler must make. | `cargo test -p ozma-node --test parity release_on_disconnect_contract_clears_held_input`, `cargo test -p ozma-input` | No session/transport disconnect handler is wired to the compat UDP listener yet, so "release on disconnect" is not end-to-end asserted. |
| Input — Linux uinput sink | `partial` | HID→Linux keycode mapping (letters, digits, controls, F1–F24, keypad, media, modifiers), absolute/relative pointer modes, buttons/wheel, duplicate suppression, `ReleaseAll`, and rejection of touch/pen/clipboard. | `cargo test -p ozma-input` (70 passed, 1 ignored) | Real `/dev/uinput` test is `#[ignore]` and needs `OZMA_UINPUT_TEST=1` plus group/udev access. |
| Input — QMP sink | `partial` | `InputEvent` → QMP `input-send-event` translation, modifier diffing, buttons, wheel press/release pairs, `ReleaseAll`, selection-chain fallback, a fake-QMP socket integration test, and two shared modes: `QmpSink::spawn_with_shared` writes through the node's single `SharedQmpClient` (serialized with power/capture/snapshots), while `QmpSink::spawn_dedicated_input` uses only a provisioned `--qmp-input-socket` and never opens a control client. The node prefers the dedicated socket for input when configured, otherwise the shared control client. | `cargo test -p ozma-input`, `cargo test -p ozma-qmp`, `cargo test -p ozma-node --test shared_qmp` | Horizontal wheel, touch/pen, and clipboard are rejected rather than faked; no real QEMU input test in CI. |
| Capture — QMP screendump | `partial` | `QmpDisplayProvider` enumerates a socket, polls `screendump`, parses binary P6 PPM to RGB24, paces at `max_fps`, and returns typed errors; provider is registered by the VM profile when `--qmp-socket` is set. The node registers it with `with_qmp_display_shared`, so HLS and snapshot sources call the one `SharedQmpClient` instead of opening another control connection; `open` still never connects. | `cargo test -p ozma-capture` (unit + QMP integration), `cargo test -p ozma-node --test shared_qmp` | Polling path with file I/O latency; captures QEMU's default console only (no `device` argument yet); live QEMU re-run pending. |
| Capture — Looking Glass KVMFR | `partial` | KVMFR header/frame parsing, format conversion (BGRA/RGBA/RGB24/NV12), geometry/bounds rejection, `LookingGlassSource`, and a side-effect-free `LookingGlassProvider` with a temp-file fixture. | `cargo test -p ozma-capture` | No real `/dev/shm/looking-glass` test; provider is opt-in, not in `system*`. |
| Capture — v4l2loopback VirtualCapture | `partial` | Side-effect-free sysfs label discovery, `VirtualCapture` command/lifecycle with injected runner, provider open/rollback, and stop cleanup. | `cargo test -p ozma-capture` | Real v4l2loopback/ffmpeg test is `#[ignore]`; provider is opt-in. |
| Capture — VNC display (ffmpeg transition) | `partial` | `VncDisplayProvider` advertises one `CaptureKind::Vnc` source (`rgb24`, uncompressed, no HDR) from validated `--vnc-host`/`--vnc-port` without connecting. `open` returns `VncDisplaySource`, which lazily spawns ffmpeg with `-f vnc -i <host>:<port> -vf scale=<w>:<h> -pix_fmt rgb24 -f rawvideo -r <fps> -`, reads exactly `width*height*3` bytes per frame, paces PTS/sequence monotonically, bounds stderr to 4 KiB, applies a first-frame startup timeout, and maps EOF/short frames to `EndOfStream`/typed errors. The VM profile selects it from `--capture-provider vnc` or the VNC flags; HLS/WebRTC open the first registry source and accept `rgb24` unchanged. | `cargo test -p ozma-capture` (VNC argv/config/enumerate/fake-child/EOF/short-frame/stop/spawn-failure/timeout tests), `cargo test -p ozma-node` (resolver precedence + VNC registry kind) | ffmpeg VNC is a transition path, not the destination. Unauthenticated loopback VNC only (no VNC password/TLS); every frame is scaled to the configured size; no VNC audio or input; no live VNC/ffmpeg run in CI (this host's ffmpeg reports `Unknown format 'vnc'`). The `#[ignore]`d real test needs `OZMA_VNC_TEST=1`. |
| Capture — D-Bus framebuffer | `partial` | `DBusDisplayProvider`/`DBusDisplaySource` implement QEMU's p2p display path: QMP `getfd`/`add_client` with `@dbus-display`, SASL EXTERNAL, `GetAll(Console_N)`, `RegisterListener` over SCM_RIGHTS, `Scanout`/`Update` BGRA frames with stride repacking, and bounded geometry/body validation. The D-Bus wire codec reads and writes exactly `body_len` bytes with no body padding (QEMU's 285-byte `GetAll` reply previously stalled the handshake waiting for 3 non-existent padding bytes). `DBusDisplayProvider`/`DBusDisplaySource::with_shared_client` and `CaptureRegistry::with_dbus_display_shared` route `getfd`/`add_client` through the node's one `SharedQmpClient`, so `--capture-provider dbus` does not open a second QMP connection. The VM profile selects it from `--capture-provider dbus` or any of `--dbus-console`/`--dbus-width`/`--dbus-height`/`--dbus-fps` when `--qmp-socket` is set; VNC/QMP/D-Bus conflicts are rejected. The default suite injects scripted transports. | `cargo test -p ozma-capture` (D-Bus codec/source/provider tests, including unpadded-body, exact-SASL, and shared-client tests), `cargo test -p ozma-node` (resolver precedence + registry kind), live `real_qemu_dbus_display_pushes_a_frame` PASS (`OZMA_DBUS_TEST_QMP`, QEMU 11.1.3: 720x400 Scanout) | Live QEMU is opt-in, not CI: the real test is `#[ignore]` and gated by `OZMA_DBUS_TEST_QMP`, and it needs an exclusive QMP socket. The live run sampled `Scanout` only; `Update`/`Disable` are unit-tested, not live. One selected `Console_N`; only 32-bit BGRA/pixman formats; the console connection is drained after setup, but the listener is read only while `next_frame` is polled (a stalled consumer back-pressures QEMU's frame pushes); no D-Bus input path. |
| VM display model | `partial` | `VmConsole`/`VmDisplayTopology` model for QMP/D-Bus/VNC/Looking Glass consoles, primary selection, left-to-right placements, validation, and `Console_N` ids. | `cargo test -p ozma-display` (58 passed, 14 VM tests) | Model only; not wired to a provider/API yet. |
| Stream — HLS `/stream/*` | `partial` | `--hls` starts a real capture → ffmpeg `rawvideo` → HLS pipeline (`ozma-node/src/hls.rs`) and serves the output directory at `/stream/*`. `rgb24` is passed through and `bgra` is converted; other formats are rejected with a typed error. `/api/v1/connection` reports `hls: "running"` only once `stream.m3u8` exists, otherwise `"not-configured"` or `"failed"` plus `hls_error`. `/stream` is mounted outside the bearer guard (after the guard layer is applied) because `controller/api.py::stream_hls_proxy` fetches it without credentials; control endpoints stay guarded and `ServeDir` rejects path traversal. Encoder argv pins GOPs to `segment_seconds`: `-g round(fps×segment_seconds)`, software-only `-keyint_min`/`-sc_threshold 0`, and `-force_key_frames expr:gte(t,n_forced*<segment_seconds>)`, so `-hls_time` matches the configured segment duration instead of the encoder's default GOP (libx264: 250 frames = 50 s at 5 fps). The feeder is also wall-clock paced (`FramePacer`): ffmpeg's input timebase stays `-r fps`, but frames that arrive before their slot are dropped before conversion, so at most `fps` frames are written per wall-clock second. The first frame is written immediately; a late frame is written and the schedule advances by one interval, with catch-up clamped so a stall cannot bank a burst. The clock is injectable (`HlsClock`) for deterministic tests. | `cargo test -p ozma-node --test parity health_and_connection_surface_match_baseline connection_reports_a_failed_hls_pipeline_with_an_error stream_files_are_served_only_from_the_configured_directory`, `cargo test -p ozma-node --lib hls::` (pacer rate, early-drop/late-emit, catch-up clamp, and EOS tests), `cargo test -p ozma-node api::tests::hls_is_served_without_bearer_but_control_stays_guarded` | No real ffmpeg or live controller proxy in the default suite (real-ffmpeg test is `#[ignore]`, `OZMA_FFMPEG_TEST=1`). `-hls_flags delete_segments` is passed. Keyframes are pinned to segment boundaries (`-g`, `-keyint_min`, `-sc_threshold 0` on software; `-g` and `-force_key_frames` on hardware). The live post-fix run still showed playlist media time advancing ~6× wall time because the pacer did not exist then; `docs/gate2-live-qmp-report.md` §0.9 records the fix and states that a live QEMU re-run is still required to confirm ~1 playlist segment per wall-clock second (and ~1-second `#EXTINF`/`#EXT-X-TARGETDURATION`). |
| Stream — WebRTC `/webrtc/offer`, `/webrtc/bitrate` (external helper) | `partial` | `--webrtc-helper` supervises the existing GStreamer/Python worker: argv `[program] [script] --sock --shm --width --height --fps`, a bounded wait for the Unix socket, newline-delimited JSON signaling (`{"type":"offer","sdp":...}` → `{"type":"answer","sdp":...}`), and an fps-paced feeder that converts `rgb24`/`bgra` frames to packed BGRA and replaces `shm_path` atomically. Other formats are typed errors. WebRTC is disabled without a helper (`not-configured`); a configured helper that fails is `failed` with `webrtc_error`, never `running`. `/webrtc/offer` and `/webrtc/bitrate` are mounted through the compat extra router behind the bearer guard; without a helper they answer typed 503s. `bitrate` sends `{"type":"bitrate","bitrate":<bps>}` and requires an acknowledgement: the worker clamps to 500 kbps–50 Mbps, applies the property to live encoders, and replies `{"ok":true,"bitrate":<bps>}`; the route returns success only for that ack and a typed 502 when the helper ignores or rejects the message. **Live (2026-11-24):** a GStreamer peer completed ICE/DTLS and decoded 302 H.264 frames (720x400) plus 1020 Opus buffers (48 kHz stereo); `/webrtc/bitrate` returned `{"ok":true,"bitrate":4000000}` and clamped 100 kbps→500 kbps and 60 Mbps→50 Mbps. See [`gate2-live-webrtc-report.md`](gate2-live-webrtc-report.md). | `cargo test -p ozma-node --lib webrtc::` (20 passed, 2 ignored), `cargo test -p ozma-node --test parity webrtc_`, `cargo test -p ozma-node --lib controller_compat::tests::webrtc_`, `python3 softnode/gst_webrtc_worker_test.py` (22 passed), live `python3 scripts/gate2/webrtc-client.py --node-url ... --audio --bitrate 4000000` | Helper-first only: no native `webrtc-rs`/`str0m`, no RTCP/rate measurement (bitrate ack only proves the encoder property was accepted), and ICE is non-trickle host candidates embedded in SDP after a bounded gather (no STUN/TURN). The live peer was GStreamer, not a browser: controller `/api/v1/streams/{node}/webrtc`, browser ICE/DTLS, and dashboard media remain unvalidated. A peer that closes while `appsrc` still accepts pushes can leave a session/encoder alive. WebRTC still opens its own capture *source* from the same registry; a QMP source reuses the node's one shared control client, but a non-QMP source that cannot be opened twice alongside HLS (for example a single-client VNC server) reports `failed` rather than sharing a frame tap. The real-helper start/stop tests are `#[ignore]` and gated by `OZMA_WEBRTC_GS_SCRIPT`; the full live run is in `docs/gate2-live-webrtc-report.md`. |
| Power — state/action endpoints | `partial` | `ozma-power` `QmpPower` and `LibvirtPower` (virsh argv/status/fallbacks), `/power/state`, `/power/{action}`, `/api/v1/connection` power summary, bearer guard, and fake-backend/fake-QMP tests. `QmpPower::with_shared_client` lets the node use the same `SharedQmpClient` as QMP capture, snapshots, and input; the owned `connect`/`with_client` constructors remain. | `cargo test -p ozma-power` (unit + fake-QMP), `cargo test -p ozma-node --test shared_qmp` | No real QEMU/libvirt test; QMP `stop` is a pause, so force-off falls back to ACPI powerdown. |
| Audio — named PipeWire target/monitor | `partial` | `AudioTarget { Default, Object, NodeId, SinkMonitor }`, `<sink>.monitor` naming/validation, `PipeWireCapture::open_target/open_sink_monitor`, and `StreamTarget::monitor_for_sink`. `ozma-softnode` now creates/reuses per-VM null sinks through `ozma-audio`'s `AudioRouter`, but capture still targets the monitor by name. | `cargo test -p ozma-audio -p ozma-pipewire` (92 passed, 1 ignored), `cargo test -p ozma-softnode` | No live PipeWire/PulseAudio server or `pactl` in CI; no native PipeWire node creation; no decoded-audio assertion. |
| Audio — per-VM sink monitor, non-silent decode, mic path | `partial` | `ozma-softnode --provision-audio --audio-backend pipewire|pulseaudio` creates `<prefix><vm>` (default `ozma-<vm>`) as a 48 kHz stereo null sink with `<sink>.monitor`, passes `--audio-sink` to `ozma-node`, and injects `PULSE_SINK` plus `<audio type='pulseaudio'/>` when provisioning libvirt XML. Existing sinks are reused. Sink ownership is persisted in `--audio-state-file` (default `$XDG_RUNTIME_DIR/ozma/audio-provision.json`, fallback `/run/ozma/audio-provision.json`) as versioned JSON with atomic writes; only sinks the router reported creating and the state records as owned are unloaded on VM removal or shutdown, including after a supervisor restart. An existing unrecorded sink is reused but never claimed, a recorded-but-missing sink is cleared on release, and persistence failures are logged without failing VM startup. Failure is non-fatal and omits the flag/env. Registration still reports `audio_type=pipewire` because `ozma-node` has no `--audio-type` flag. | `cargo test -p ozma-softnode` (fake `AudioRouter`/`AudioSinkProvisioner` plus a temporary state file; no real `pactl`) | Still deferred: audible/non-silent output validation, decoded-audio assertion, mic path, native PipeWire node creation, live supervisor restart against a real audio server/sink, and live server/controller validation. |
| VM supervision — per-VM `ozma-node` process, ports, restart | `partial` | `ozma-softnode` supervises real `ozma-node --profile vm` subprocesses: deterministic HID/HTTP ports, QMP/capture/power/input flags, optional `--audio-sink`, single-socket QMP input fallback, start/stop/restart with bounded backoff, unchanged-VM reuse, shutdown/reap, and promotion after a VM gains a socket. | `cargo test -p ozma-softnode` (90 passed) | Subprocess transition only; no real QEMU/libvirt test. |
| VM provisioning — QMP/D-Bus/evdev/audio, managed without edits | `partial` | Opt-in `--provision` on libvirt: `virsh dumpxml --inactive`, dirty-XML-safe `<qemu:commandline>` injection (QMP control/input/display chardevs, `-display dbus,p2p=yes`, evdev `input-linux` from explicit paths **or** `--provision-evdev`-created uinput devices, `PULSE_SERVER`/`XDG_RUNTIME_DIR`/`PULSE_SINK` env and exact `<audio id='1' type='none'/>` → `pulseaudio`), idempotent re-injection, qemu namespace insertion, `virsh define /dev/stdin`, typed errors, Proxmox refusal, and opt-in `virsh destroy`/`start` with a bounded socket wait. Created evdev devices are kept open per VM, marker files/symlinks are written, and devices are released on stop/removal. `--provision-audio` creates/reuses per-VM sinks before the node starts, persists sink ownership in `--audio-state-file` across supervisor restarts, and removes only recorded owned sinks. Every creation/route failure logs and falls back; no path/sink is fabricated. | `cargo test -p ozma-softnode` (90 passed; fake runner/virsh, fake evdev/audio provisioners, XML fixtures; no `/dev/uinput`, `pactl`, or real virsh) | Still deferred: libvirt API and devices outside the command-line block, validating that QEMU actually opened/grabbed evdev devices, live D-Bus/QEMU validation, audible sink validation, native PipeWire node creation, live supervisor restart against a real sink, USB/media, Looking Glass, and real libvirt/QEMU restart validation. Editing XML is opt-in and can require a VM restart (`--provision-restart`). |
| Native plane — workspace, QUIC/WebTransport | `green` | Full workspace test suite, including existing QUIC/WebTransport and browser-capture tests, passed. | `cargo test --workspace` (1413 passed, 0 failed, 16 ignored) | Keep green. Hardware-dependent tests remain `#[ignore]`/opt-in. |
| Regression — controller pytest, demo scenario, clean-host install | `not-run` | Not executed in this Phase 0 work. | — | Run the unmodified controller pytest suite, the demo, and a clean-host install before claiming Gate 2/3. |

## How to run

Default, CI-safe (no hardware, no Python, no fixed ports):

```bash
cargo test -p ozma-node --test parity
cargo test -p ozma-node --lib
cargo test -p ozma-qmp -p ozma-input -p ozma-power
cargo test -p ozma-capture -p ozma-display -p ozma-audio
cargo test --workspace
```

Phase 1/2 additions and their evidence:

| Workstream | Evidence |
|---|---|
| `ozma-qmp` dual-socket client + HID→QMP | `cargo test -p ozma-qmp` (62 passed, 1 ignored) |
| `ozma-input` uinput + QMP sinks + selection chain + reverse keymap | `cargo test -p ozma-input` (64 unit + 14 integration + 2 doctests, 1 ignored) |
| `ozma-power` QMP + libvirt backends | `cargo test -p ozma-power` (34 passed, 1 ignored) |
| `ozma-capture` QMP/LookingGlass/VirtualCapture/VNC/D-Bus | `cargo test -p ozma-capture` (122 unit + 5 QMP integration; live D-Bus test PASS with `OZMA_DBUS_TEST_QMP`) |
| `ozma-display` VM console model | `cargo test -p ozma-display` (58 passed) |
| `ozma-audio` named PipeWire target + routing | `cargo test -p ozma-audio -p ozma-pipewire` (92 passed, 1 ignored) |
| `ozma-node` VM profile + `/power/*` + compat input/codec/audio | `cargo test -p ozma-node` (183 passed, 3 ignored) |
| `ozma-node` local HLS pipeline + `/stream/*` | `cargo test -p ozma-node --lib hls::` (28 passed, 1 ignored; includes GOP/keyframe argv, gop-math, and wall-clock pacer tests); `cargo test -p ozma-node --test parity` (17 passed) |
| `ozma-node` WebRTC external-helper surface | `cargo test -p ozma-node --lib webrtc::` (20 passed, 2 ignored); `cargo test -p ozma-node --test parity webrtc_` (4 passed); `cargo test -p ozma-node --lib controller_compat::tests::webrtc_` (2 passed); `python3 softnode/gst_webrtc_worker_test.py` (22 passed) |
| Live WebRTC negotiation + media | `python3 scripts/gate2/webrtc-client.py --node-url ... --audio --bitrate 4000000` → ICE `COMPLETED`/DTLS connected, 302 H.264 frames at 720x400, 1020 Opus buffers, bitrate ack; see `docs/gate2-live-webrtc-report.md` |
| `ozma-softnode` per-VM subprocess supervisor + QMP/evdev/audio provisioning | `cargo test -p ozma-softnode` (90 passed) |

The parity target prints one test per row above:

```text
registration_payload_matches_python_controller_baseline
registration_txt_cannot_overwrite_the_resolved_host
minimal_registration_keeps_the_required_controller_fields
fake_controller_receives_registration_and_heartbeat
hid_v1_decoder_matches_python_translation_baseline
udp_hid_listener_records_and_injects_baseline_events
release_on_disconnect_contract_clears_held_input
health_and_connection_surface_match_baseline
display_snapshot_and_info_match_baseline
legacy_routes_report_typed_501s_and_minimal_hints
agent_metrics_is_a_flat_controller_metrics_document
public_audio_discovery_and_sweep_are_typed_501s
connection_reports_a_failed_hls_pipeline_with_an_error
codec_override_shape_matches_baseline_and_reports_pending_restart
audio_routes_remain_typed_501_without_a_router
webrtc_routes_report_not_configured_rather_than_404
webrtc_routes_report_a_failed_helper_without_claiming_running
webrtc_offer_proxies_to_a_fake_unix_socket_helper
webrtc_bitrate_is_not_reported_as_success_without_an_ack
stream_files_are_served_only_from_the_configured_directory
```

The default parity target does not run ffmpeg or a GStreamer/Python helper. The
HLS unit tests inject a recording `FrameSink`/`FfmpegSpawner`; the WebRTC unit
and parity tests inject a fake `HelperSpawner`/`Signaler`. The real-helper and
real-ffmpeg tests are `#[ignore]`:

```bash
OZMA_FFMPEG_TEST=1 cargo test -p ozma-node --lib hls::tests::real_ffmpeg_writes_a_playlist -- --ignored
OZMA_WEBRTC_GS_SCRIPT=/path/to/gst_webrtc_worker.py \
  cargo test -p ozma-node --lib webrtc::tests::real_gstreamer_helper_starts_and_stops -- --ignored
```

Fixtures:

| File | Baseline |
|---|---|
| `ozma-node/tests/parity/registration_baseline.json` | `node/node.py` `run()` TXT and `_direct_register()`; `controller/api.py` `DirectRegisterRequest` and `register_node()` |
| `ozma-node/tests/parity/hid_v1_baseline.json` | `softnode/evdev_input.py` `EvdevHIDTranslator.handle_keyboard` / `handle_mouse` / `handle_mouse_relative` |
| `ozma-node/tests/parity/http_baseline.json` | Controller-facing `/health` and `/api/v1/connection` surface, including the opt-in HLS and WebRTC status tokens (`not-configured`/`running`/`failed`) and their `*_error` fields |
| `ozma-node/tests/parity/README.md` | Source notes and the future-work statement |

The fixtures are hand-written from the Python baseline. They are **not**
regenerated from Rust; doing so would only prove Rust equals Rust.

### Opt-in real-hardware / Python runs (not in CI)

These are intentionally not enabled by default because they need privileges or
external processes:

- **Real HID gadget:** `/dev/hidg0`, `/dev/hidg1`, udev/AppArmor/cgroup access.
  The CI path uses `RecordingSink<FakeInputSink>` and the `stub-gadget` build.
- **Real QEMU/libvirt:** QMP sockets, D-Bus framebuffer, VNC, KVMFR. Phase 1
  tests should be marked `#[ignore]` or environment-gated, with a fake-QMP
  fixture for the default run.
- **Real VNC:** ffmpeg must be built with the VNC demuxer (the host ffmpeg used
  for this slice reports `Unknown format 'vnc'`) and a QEMU VNC endpoint must be
  listening. Run `OZMA_VNC_TEST=1 [OZMA_VNC_HOST=…] [OZMA_VNC_PORT=…] cargo
  test -p ozma-capture -- --ignored real_vnc_ffmpeg_captures_a_frame`.
- **Real WebRTC helper:** GStreamer + the Python worker from `softnode/`. Set
  `OZMA_WEBRTC_GS_SCRIPT=/path/to/gst_webrtc_worker.py` (and optionally
  `OZMA_WEBRTC_GS_PYTHON`) and run the `#[ignore]`d
  `webrtc::tests::real_gstreamer_helper_starts_and_stops` test. A live
  controller negotiation (`POST /api/v1/streams/{node}/webrtc`) is not run in
  CI.
- **Live Python comparison:** start `node/node.py` (or the
  `softnode/evdev_input.py` translator) against the same fake controller and
  diff its captured JSON/events against these fixtures. This remains future
  work; see `ozma-node/tests/parity/README.md`.
- **Controller regression:** run the unmodified controller pytest suite and the
  demo scenario against a Rust node before marking the regression row green.

## Fake QMP support (implemented)

`ozma-qmp/tests/common/` provides a loopback Unix-socket server that sends the
`{"QMP": {...}}` greeting, answers `qmp_capabilities`, records
`screendump`/`input-send-event`/power commands, and exercises SCM_RIGHTS
`getfd`. `ozma-capture/tests/qmp_display.rs` and
`ozma-power/tests/qmp_power.rs` reuse the same protocol shape. Real QEMU tests
remain `#[ignore]`/environment-gated.

## Remaining Gate 2 gaps

- Native WebRTC (`webrtc-rs`/`str0m`) is not implemented. `/webrtc/offer` and
  `/webrtc/bitrate` run through a supervised external GStreamer helper; bitrate
  changes are not honoured by the current worker and surface as typed 502s.
  WebRTC audio is wired through the helper CLI (`--webrtc-audio-source`,
  `--webrtc-audio-backend`) and reported as configuration intent, but no live
  monitor/browser audio negotiation or sample production is validated. No live
  helper/controller negotiation is run in CI.
- Controller display snapshots are implemented as a one-shot
  `GET /display/snapshot` (real JPEG from the configured capture source) with
  `GET /display/info` and a guarded typed 501 `GET /display/mjpeg`. Snapshot,
  info, hints, node status, `/agent/metrics`, and the audio discovery/sweep
  stubs are merged after the bearer guard so the unmodified controller works
  with `--api-token`; control routes remain guarded. No live
  QMP/VNC/D-Bus/camera or controller dashboard preview has been run in this
  workspace, and the live controller's snapshot/metrics/hints/status polls have
  not been exercised against a real node process.
- Live-controller HLS proxy run: the node pipeline and `/stream/*` are
  unit/route tested, but the unmodified controller has not proxied a Rust node's
  playlist in this workspace.
- Runtime codec override (`POST /codec`) validates and stores an
  `EncoderOverride` plus `OverrideReceipt`, and restarts a running HLS
  `HlsPipeline` with the mapped encoder. Software (`libx264`, `libx265`,
  `libsvtav1`, `libaom-av1`, `mjpeg`) and H.264/HEVC hardware
  (`vaapi`/`nvenc`/`qsv`/`v4l2m2m`) are supported; `applied: true` means that
  restart succeeded. Hardware availability is evidence from an injectable
  `EncoderProbe` (production runs bounded `ffmpeg -encoders` once), and a
  missing VAAPI render node is a typed error with remediation. A hardware
  request never silently becomes software: an explicit
  `allow_software_fallback` produces `fallback: true` with the requested and
  applied encoders, otherwise the result is a typed 502 and the previous
  pipeline is restored. With HLS off — or for a failed restart — the honest
  result remains `applied: false` / `pending_restart: true` (or a typed 502).
  Hardware AV1/MJPEG, `videotoolbox`, and WebRTC codec switching are not
  implemented; WebRTC keeps its existing encoder. No real GPU/ffmpeg hardware
  encode runs in CI, so driver-specific behaviour (VAAPI upload, NVENC/QSV
  rate control, V4L2M2M device open) still needs opt-in hardware validation.
  Audio routing (`/audio/*`) is wired to a real `AudioRouter` when
  `--audio-router pipewire|pulseaudio` is set, but there is no native DSP/room
  correction, no PipeWire node enumeration for `GET /audio/nodes`, and no
  `POST /audio/sweep` implementation; both return public typed 501 stubs with
  the soft-node response shape. There is no live audio-server run in CI.
  Per-VM sink creation belongs to `ozma-softnode --provision-audio`;
  `ozma-node` only receives the sink name.
  Without a router both still answer typed 501s/503s rather than fake success.
- D-Bus framebuffer display provider is implemented and selectable on the VM
  profile. It was live-validated against QEMU 11.1.3 (`Scanout` 720x400, plus a
  node HLS run over the shared QMP connection) on 2026-11-24, but that path is
  not in CI: the real test is `#[ignore]`, gated by `OZMA_DBUS_TEST_QMP`, and
  needs an exclusive QMP socket. `Update`/`Disable`, live resolution changes,
  multiple consoles, non-BGRA formats, and D-Bus input remain unverified or
  unimplemented, and a stalled `next_frame` consumer back-pressures QEMU. The
  ffmpeg VNC path remains a transition path: unauthenticated loopback VNC only,
  scaled output, no VNC audio/input, and no live VNC validation in CI.
- `ozma-softnode` provisioning: libvirt API/device creation beyond the
  `qemu:commandline` block, validating that QEMU opened/grabbed created evdev
  devices, audible/non-silent sink validation, native PipeWire node creation,
  live supervisor-restart validation against a real audio server/sink, USB/media,
  Looking Glass, and a live libvirt/QEMU restart test. The subprocess supervisor,
  port allocation, restart/backoff, opt-in QMP/display XML injection, env/evdev
  injection, Linux uinput device creation, per-VM null-sink creation/removal
  with persisted ownership, and bounded restart wait are implemented and tested
  with fakes.
- Real-QEMU Gate 2 demo and controller pytest/demo/clean-host regression runs.

## Related documents

- [`docs/rust-reconciliation-plan.md`](rust-reconciliation-plan.md) — phases,
  Gate 0, parity gates and tests, risks.
- [`docs/node-hermes-parity.md`](node-hermes-parity.md) — M0 contract, testing
  strategy, acceptance criteria.
