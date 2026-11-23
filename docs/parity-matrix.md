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
  **11 passed, 0 failed**; `cargo test -p ozma-node` → **126 passed,
  0 failed, 1 ignored**; `cargo test --workspace` → **1159 passed, 0 failed,
  11 ignored**.
- Fake QMP support: `ozma-qmp/tests/common/` provides a greeting/capabilities
  server and an SCM_RIGHTS `getfd` peer. `ozma-capture/tests/qmp_display.rs`
  and `ozma-power/tests/qmp_power.rs` reuse the same protocol shape for their
  provider tests. No real QEMU is needed for the default suite.

## Matrix

| Area | Status | What is asserted today | Evidence | Gap / next gate |
|---|---|---|---|---|
| Registration — mDNS TXT shape | `partial` | `registration_payload` produces the controller's `DirectRegisterRequest` fields from the mDNS TXT map: required `id`/`host`/`port`, string-typed controller fields, ports and JSON lists, `machine_class`, `capture_device`, `display_outputs`, `pci_devices`, `audio_type`, `audio_sink`, `vm_guest_ip`, `stream_port`/`stream_path`. TXT `host` cannot replace the resolved host. | `cargo test -p ozma-node --test parity registration_` (3 tests) | No test browses/advertises real mDNS. `mdns::advertise` and controller-side discovery remain untested here. |
| Registration — direct POST + heartbeat | `green` | The real `registration::register` POSTs to an in-process axum fake controller; the captured JSON equals `tests/parity/registration_baseline.json`. The real `heartbeat_loop` sends `{"node_id": ...}` and the fake controller captures it. | `cargo test -p ozma-node --test parity fake_controller_receives_registration_and_heartbeat` | The peer is a fixture-shaped fake, not the live Python controller. Live controller POST is a later opt-in run. |
| HTTP compat surface — `/health`, `/api/v1/connection`, `/power/*` | `green` | Keys and JSON types match `tests/parity/http_baseline.json`; health reports `ok`, node id, protocol major, session count; connection reports ports, sink kind, HID stats, power summary, input sink availability/injection, codec state, `hls` (`not-configured`/`running`/`failed`), `hls_error`, `audio`, and `webrtc`; `/power/state` and `/power/{action}` are bearer-guarded and covered with a fake backend. | `cargo test -p ozma-node --test parity health_and_connection_surface_match_baseline connection_reports_a_failed_hls_pipeline_with_an_error`, `cargo test -p ozma-node` (compat tests) | WebRTC routes remain absent. `/stream/*` auth behavior is asserted in the `ozma-node` API unit test, not yet against the live controller. |
| HTTP compat surface — `/input/key`, `/input/mouse`, `/input/type` | `green` | Key requests take Python-compatible Linux evdev `keycode` + `down` and map through `ozma_input::linux_keycode_to_hid_usage` into `InputEvent::Key` on the one shared sink; unknown/out-of-range keycodes are typed 400s and a duplicate press is accepted without changing held state. Mouse requests take `x`/`y` in `0..=32767`, `action` (`move`/`press`/`release`/`click`), and button `0`/`1`→left, `2`→right, `4`→middle, `8`→back, `16`→forward; click emits press, a 20 ms hold, and release. Typing maps printable ASCII, newline, and tab to HID usages with an explicit left shift, rejects non-ASCII/oversized text (4096 chars), and sleeps 15 ms between characters. Responses include `"injected": false` for a recording-only sink; a missing sink is 503. | `cargo test -p ozma-node --lib controller_compat::tests::input_`, `cargo test -p ozma-node --lib controller_compat::tests::text_mapping_` | No real QMP/uinput/gadget injection in CI; text is US-layout ASCII only; click/typing delays are fixed bounds rather than guest-acknowledged pacing. |
| HTTP compat surface — `/codec` | `partial` | `GET /codec` reports `current_encoder`, `override: null`, and `available` derived from the node's advertised video encode capabilities; a node without video returns `null`/empty rather than inventing an encoder. `POST /codec` returns a typed 501 (`codec override is not implemented`) with remediation. | `cargo test -p ozma-node --lib controller_compat::tests::codec_route_` | No runtime encoder switching: applying an override requires restarting the capture/encode pipeline and is deliberately not faked. |
| Audio — `/audio/apply`, `/audio/remove`, `/audio/play`, `/audio/stop` compatibility | `not-implemented` | Every route returns a typed 501 `{ok:false, code:"audio-routing-unsupported", error:"audio routing is not implemented on this node"}` plus remediation instead of a success-shaped no-op; `/api/v1/connection` reports `audio: "not-implemented"`. | `cargo test -p ozma-node --lib controller_compat::tests::audio_routes_are_explicitly_not_implemented` | PipeWire routing, room-correction profile apply/remove, reference-track playback, and per-VM audio remain unimplemented. The UAC2 gadget bridge is not this surface. |
| Input — HID v1 keyboard | `green` | Keyboard press, release, multiple keys, duplicate report suppression, and modifier press/release produce the normalized `InputEvent` sequence in `tests/parity/hid_v1_baseline.json`. | `cargo test -p ozma-node --test parity hid_v1_decoder_matches_python_translation_baseline` | None for v1 decode. Python baseline maps HID→evdev in the sink; the fixture asserts the normalized adapter output. |
| Input — HID v1 pointer (absolute/relative/buttons/wheel) | `green` | Absolute and relative motion, button press/release, and wheel sign match the fixture. The real `HidUdpListener` on an ephemeral port decodes and submits events through `RecordingSink<FakeInputSink>`. | `cargo test -p ozma-node --test parity hid_v1_decoder_matches_python_translation_baseline udp_hid_listener_records_and_injects_baseline_events` | No real HID gadget or evdev injection in CI; that is the hardware opt-in path. |
| Input — release on disconnect | `partial` | The `InputSink::release_all` contract clears held keys/buttons and records `ReleaseAll` for the gadget, uinput, QMP, and recording sinks; this is the call a disconnect handler must make. | `cargo test -p ozma-node --test parity release_on_disconnect_contract_clears_held_input`, `cargo test -p ozma-input` | No session/transport disconnect handler is wired to the compat UDP listener yet, so "release on disconnect" is not end-to-end asserted. |
| Input — Linux uinput sink | `partial` | HID→Linux keycode mapping (letters, digits, controls, F1–F24, keypad, media, modifiers), absolute/relative pointer modes, buttons/wheel, duplicate suppression, `ReleaseAll`, and rejection of touch/pen/clipboard. | `cargo test -p ozma-input` (70 passed, 1 ignored) | Real `/dev/uinput` test is `#[ignore]` and needs `OZMA_UINPUT_TEST=1` plus group/udev access. |
| Input — QMP sink | `partial` | `InputEvent` → QMP `input-send-event` translation, modifier diffing, buttons, wheel press/release pairs, `ReleaseAll`, selection-chain fallback, and a fake-QMP socket integration test. | `cargo test -p ozma-input`, `cargo test -p ozma-qmp` | Horizontal wheel, touch/pen, and clipboard are rejected rather than faked; no real QEMU input test in CI. |
| Capture — QMP screendump | `partial` | `QmpDisplayProvider` enumerates a socket, polls `screendump`, parses binary P6 PPM to RGB24, paces at `max_fps`, and returns typed errors; provider is registered by the VM profile when `--qmp-socket` is set. | `cargo test -p ozma-capture` (87 unit + 5 QMP integration) | Polling path with file I/O latency; captures QEMU's default console only (no `device` argument yet). |
| Capture — Looking Glass KVMFR | `partial` | KVMFR header/frame parsing, format conversion (BGRA/RGBA/RGB24/NV12), geometry/bounds rejection, `LookingGlassSource`, and a side-effect-free `LookingGlassProvider` with a temp-file fixture. | `cargo test -p ozma-capture` | No real `/dev/shm/looking-glass` test; provider is opt-in, not in `system*`. |
| Capture — v4l2loopback VirtualCapture | `partial` | Side-effect-free sysfs label discovery, `VirtualCapture` command/lifecycle with injected runner, provider open/rollback, and stop cleanup. | `cargo test -p ozma-capture` | Real v4l2loopback/ffmpeg test is `#[ignore]`; provider is opt-in. |
| Capture — D-Bus framebuffer / VNC | `not-implemented` | No Rust D-Bus or VNC display source. | — | Add `DBusDisplayProvider`/`VncDisplayProvider` in a later Phase 1/2 slice. |
| VM display model | `partial` | `VmConsole`/`VmDisplayTopology` model for QMP/D-Bus/VNC/Looking Glass consoles, primary selection, left-to-right placements, validation, and `Console_N` ids. | `cargo test -p ozma-display` (58 passed, 14 VM tests) | Model only; not wired to a provider/API yet. |
| Stream — HLS `/stream/*` and WebRTC controller proxy | `partial` | `--hls` starts a real capture → ffmpeg `rawvideo` → HLS pipeline (`ozma-node/src/hls.rs`) and serves the output directory at `/stream/*`. `rgb24` is passed through and `bgra` is converted; other formats are rejected with a typed error. `/api/v1/connection` reports `hls: "running"` only once `stream.m3u8` exists, otherwise `"not-configured"` or `"failed"` plus `hls_error`; WebRTC stays `"not-implemented"` and `/webrtc/*` stays 404. `/stream` is mounted outside the bearer guard (after the guard layer is applied) because `controller/api.py::stream_hls_proxy` fetches it without credentials; control endpoints stay guarded and `ServeDir` rejects path traversal. | `cargo test -p ozma-node --test parity health_and_connection_surface_match_baseline connection_reports_a_failed_hls_pipeline_with_an_error stream_files_are_served_only_from_the_configured_directory webrtc_routes_are_absent_rather_than_faked`, `cargo test -p ozma-node --lib hls::`, `cargo test -p ozma-node api::tests::hls_is_served_without_bearer_but_control_stays_guarded` | No real ffmpeg or live controller proxy in the default suite (real-ffmpeg test is `#[ignore]`, `OZMA_FFMPEG_TEST=1`). HLS playlist advance/segment rotation against a live QEMU VM is not asserted; the fixed argv does not pass `-hls_flags delete_segments`, so retained segments stay on disk. WebRTC (`/webrtc/offer`, `/webrtc/bitrate`) remains missing. |
| Power — state/action endpoints | `partial` | `ozma-power` `QmpPower` and `LibvirtPower` (virsh argv/status/fallbacks), `/power/state`, `/power/{action}`, `/api/v1/connection` power summary, bearer guard, and fake-backend/fake-QMP tests. | `cargo test -p ozma-power` (34 passed, 1 ignored), `cargo test -p ozma-node` | No real QEMU/libvirt test; QMP `stop` is a pause, so force-off falls back to ACPI powerdown. |
| Audio — named PipeWire target/monitor | `partial` | `AudioTarget { Default, Object, NodeId, SinkMonitor }`, `<sink>.monitor` naming/validation, `PipeWireCapture::open_target/open_sink_monitor`, and `StreamTarget::monitor_for_sink`. | `cargo test -p ozma-audio -p ozma-pipewire` (92 passed, 1 ignored) | No sink/monitor creation or routing, no live PipeWire test, no per-VM audio provisioning. |
| Audio — per-VM sink monitor, non-silent decode, mic path | `not-implemented` | Only registration fields (`audio_type`, `audio_sink`) are asserted. The UAC2 gadget bridge is not a VM audio-routing parity test. | `cargo test -p ozma-node --test parity registration_payload_matches_python_controller_baseline` (fields only) | Phase 2: per-VM sink routing, decoded-audio assertion, mic path. |
| VM supervision — per-VM `ozma-node` process, ports, restart | `partial` | `ozma-softnode` supervises real `ozma-node --profile vm` subprocesses: deterministic HID/HTTP ports, QMP/capture/power/input flags, single-socket QMP input fallback, start/stop/restart with bounded backoff, unchanged-VM reuse, and shutdown/reap. | `cargo test -p ozma-softnode` (20 passed) | Subprocess transition only; no real QEMU/libvirt test. |
| VM provisioning — QMP/D-Bus/evdev/audio, managed without edits | `not-implemented` | No QMP/display socket creation, libvirt XML edits, D-Bus p2p, evdev `input-linux`, or per-VM audio routing. | — | Port `_provision_vm` and the remaining device/audio provisioning into `ozma-softnode`. |
| Native plane — workspace, QUIC/WebTransport | `green` | Full workspace test suite, including existing QUIC/WebTransport and browser-capture tests, passed. | `cargo test --workspace` (1159 passed, 0 failed, 11 ignored) | Keep green. Hardware-dependent tests remain `#[ignore]`/opt-in. |
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
| `ozma-capture` QMP/LookingGlass/VirtualCapture | `cargo test -p ozma-capture` (87 unit + 5 QMP integration) |
| `ozma-display` VM console model | `cargo test -p ozma-display` (58 passed) |
| `ozma-audio` named PipeWire target | `cargo test -p ozma-audio -p ozma-pipewire` (92 passed, 1 ignored) |
| `ozma-node` VM profile + `/power/*` + compat input/codec | `cargo test -p ozma-node` (126 passed, 1 ignored) |
| `ozma-node` local HLS pipeline + `/stream/*` | `cargo test -p ozma-node --lib hls::` (11 passed, 1 ignored); `cargo test -p ozma-node --test parity` (11 passed) |
| `ozma-softnode` per-VM subprocess supervisor | `cargo test -p ozma-softnode` (20 passed) |

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
connection_reports_a_failed_hls_pipeline_with_an_error
webrtc_routes_are_absent_rather_than_faked
stream_files_are_served_only_from_the_configured_directory
```

The default parity target does not run ffmpeg. The HLS unit tests inject a
recording `FrameSink`/`FfmpegSpawner`; the one real-ffmpeg test is `#[ignore]`:

```bash
OZMA_FFMPEG_TEST=1 cargo test -p ozma-node --lib hls::tests::real_ffmpeg_writes_a_playlist -- --ignored
```

Fixtures:

| File | Baseline |
|---|---|
| `ozma-node/tests/parity/registration_baseline.json` | `node/node.py` `run()` TXT and `_direct_register()`; `controller/api.py` `DirectRegisterRequest` and `register_node()` |
| `ozma-node/tests/parity/hid_v1_baseline.json` | `softnode/evdev_input.py` `EvdevHIDTranslator.handle_keyboard` / `handle_mouse` / `handle_mouse_relative` |
| `ozma-node/tests/parity/http_baseline.json` | Controller-facing `/health` and `/api/v1/connection` surface, including the opt-in HLS status tokens (`not-configured`/`running`/`failed`) and `hls_error` |
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

- WebRTC `/webrtc/offer`/`/webrtc/bitrate` (GStreamer helper first, native later).
- Live-controller HLS proxy run: the node pipeline and `/stream/*` are
  unit/route tested, but the unmodified controller has not proxied a Rust node's
  playlist in this workspace.
- Runtime codec override (`POST /codec`) and audio routing/correction
  (`/audio/apply`, `/audio/remove`, `/audio/play`, `/audio/stop`). `GET /codec`
  and the input compatibility routes are wired; the remaining endpoints answer
  typed 501s until a real encode/restart and PipeWire path exist.
- D-Bus framebuffer and VNC display providers.
- `ozma-softnode` provisioning: QMP/display socket creation, libvirt XML edits,
  D-Bus p2p, evdev `input-linux`, per-VM audio routing, and status/metrics
  aggregation. The subprocess supervisor, port allocation, and restart/backoff
  are implemented.
- Real-QEMU Gate 2 demo and controller pytest/demo/clean-host regression runs.

## Related documents

- [`docs/rust-reconciliation-plan.md`](rust-reconciliation-plan.md) — phases,
  Gate 0, parity gates and tests, risks.
- [`docs/node-hermes-parity.md`](node-hermes-parity.md) — M0 contract, testing
  strategy, acceptance criteria.
