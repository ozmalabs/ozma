# Plan execution status

Tracking the implementation of [`node-hermes-parity.md`](node-hermes-parity.md)
and the Rust reconciliation in
[`rust-reconciliation-plan.md`](rust-reconciliation-plan.md). This file records
what is actually merged and tested, not what is planned.

Capability and security policy (aperture) is planned in [`aperture-policy.md`](aperture-policy.md); rollout step 1 (`ozma-aperture`) is implemented, and USB/transport/capture/auth adoption remains planned.

USB video output (UVC basic + raw Ozma vendor mode) is planned in [`usb-video-plan.md`](usb-video-plan.md).

The migration from the Python `softnode/`, `node/`, and `agent/` processes to the Rust stack is planned in [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md). Parity evidence is tracked in [`parity-matrix.md`](parity-matrix.md).

## Rust reconciliation — Phase 0

| Item | Status | Evidence |
|---|---|---|
| Dead `src/` prototypes deleted | Done | `src/agent`, `src/softnode`, `src/main.rs`, and `src/streamdeck.rs` removed; they were outside the workspace and had no references |
| `ozma-agent` frozen and marked deprecated | Done | module doc, `Cargo.toml` description, and `--help` all point to `ozma-node --profile desktop` |
| `ozma-softnode` no longer spawns a nonexistent binary | Done | child-process spawn removed; auto-management fails fast with an actionable error; `--no-auto-manage` is discovery-only |
| `ozma-node --profile {hardware,vm,desktop}` | Done (configuration) | `NodeProfile` and `ProviderSelection` plus CLI overrides; `vm`/`desktop` log a configuration-only warning until Phases 1/3 |
| Controller compatibility skeleton | Done | real UDP HID v1 listener, `/api/v1/connection`, registration payload parity, bearer-guarded compat router; `cargo test -p ozma-node --lib` (69 tests) |
| Parity harness and matrix | Done | `ozma-node/tests/parity.rs` (9 tests, CI-safe fake controller) and [`parity-matrix.md`](parity-matrix.md); `cargo test --workspace` green |
| `ozma-aperture` rollout step 1 | Done | pure meet/provenance/recourse/receipt crate with policy loader; 9 integration tests plus doctest |

Phase 0 does **not** implement QMP/D-Bus/VNC capture, HLS/WebRTC compat routes,
power, VM provisioning, or real EVDI. Those remain Phase 1–3 work and are marked
`not-implemented`/`not-run` in the parity matrix.

## Rust reconciliation — Phase 1 / Phase 2

| Item | Status | Evidence |
|---|---|---|
| `ozma-qmp` QMP client | Done | dual control/input sockets, greeting/capabilities, request/response timeouts, event skipping, reconnect/backoff, `screendump`, `input-send-event`, `add_client`, `getfd`; 62 tests (1 ignored real-QEMU) |
| HID → QMP translation | Done | full `HID_TO_QCODE`, keyboard/mouse/relative state machines, wheel press/release pairs, button diffs; 32 tests |
| Linux uinput sink | Done (unit) | HID→evdev mapping, absolute/relative modes, buttons/wheel, release-all, selection vocabulary; 70 tests, real-device test ignored |
| QMP input sink | Done | `QmpSink` async writer, modifier diffing, wheel/button translation, release-all, fake-QMP integration; 70 `ozma-input` tests |
| `ozma-power` | Done (unit/fake) | `QmpPower`, `LibvirtPower`, `virsh` argv/status/fallbacks, typed errors; 34 tests (1 ignored real-QEMU) |
| QMP capture provider | Done (polling) | `QmpDisplayProvider` PPM screendump → RGB24, fps pacing, typed errors, fake-QMP integration; opt-in registry |
| Looking Glass provider | Done (fixture) | KVMFR parser/source/provider, RGBA/I420 conversion, temp shared-memory fixture; opt-in registry |
| VirtualCapture provider | Done (fake runner) | side-effect-free sysfs discovery, v4l2loopback/ffmpeg lifecycle, rollback/stop; real test ignored |
| VNC display provider | Partial (ffmpeg transition) | `VncDisplayProvider`/`VncDisplaySource` enumerate without I/O, spawn ffmpeg `-f vnc` lazily, read raw RGB24 frames with monotonic PTS/sequence, bound stderr, and map EOF/short frames to typed errors; fake-child tests; real VNC test ignored (`OZMA_VNC_TEST=1`) |
| D-Bus framebuffer provider | Not started | QMP screendump and the ffmpeg VNC transition path are the VM display capture paths today |
| VM display model | Done (model) | `VmConsole`/`VmDisplayTopology`, placements, primary/validation, `Console_N` ids; 14 tests |
| Audio target selection | Done (model) | `AudioTarget`, `<sink>.monitor` naming, `PipeWireCapture::open_target/open_sink_monitor`; no routing yet |
| VM profile wiring | Partial | `--qmp-socket`/`--qmp-input-socket`/`--libvirt-domain`, QMP capture registry, `--vnc-host`/`--vnc-port`/`--vnc-width`/`--vnc-height`/`--vnc-fps` with VNC-over-QMP precedence and explicit-provider resolution, input sink chain (single-socket QMP fallback), QMP/libvirt power, `/power/state`, `/power/{action}`, connection power summary; 123 `ozma-node` lib tests |
| HLS pipeline | Partial | `HlsPipeline` reads RGB24/BGRA `CaptureSource` frames into ffmpeg HLS, `/stream/*` serves the output dir, connection reports `running`/`failed`/`not-configured`; 112 `ozma-node` tests before the compat additions |
| Controller input compat | Done (recording/fake in CI) | `/input/key`, `/input/mouse`, `/input/type`, `GET/POST /codec`, honest `/audio/*` 501s, bearer-guarded; real injection is hardware/QMP opt-in |
| WebRTC compat | Partial (helper-first) | `WebRtcHelper` supervises an external GStreamer/Python worker over a Unix socket, feeds BGRA frames from a capture source, and proxies `/webrtc/offer`/`/webrtc/bitrate`; connection reports `running`/`not-configured`/`failed`. No native WebRTC, no WebRTC audio, no live helper in CI |
| `ozma-softnode` per-VM supervisor | Partial | supervises real `ozma-node --profile vm` subprocesses with deterministic ports, restart/backoff, stop/reap, and QMP input fallback; 20 tests |
| `ozma-softnode` provisioning | Not started | QMP/display socket creation, libvirt XML edits, D-Bus p2p, evdev `input-linux`, and per-VM audio routing remain deferred |

Gate 2 is **not** met: native/live WebRTC validation, D-Bus framebuffer
capture, live VNC validation, full VM provisioning, and a real-QEMU/controller
demo remain. The parity matrix lists the remaining rows.

## M0 — Contract and cleanup

| Item | Status | Evidence |
|---|---|---|
| Shared `ozma-protocol` crate | Done | capabilities, sessions, permissions, transports, displays, input, control, diagnostics, errors; 19 tests |
| Node builds without native OpenSSL | Done | `ozma-node` uses `reqwest` + `rustls-tls` |
| Versioned node API | Done | `/api/v1/health`, `/node`, `/capabilities`, `/diagnostics`, `/metrics`, `/sessions`, `/displays`, Hestia capability discovery |
| Fake session reaches `Ready`/`Streaming` | Done | `cargo test -p ozma-node --test m0_acceptance` |
| Fake capture/encoder/transport | Done | `ozma-media`, `ozma-capture`, `ozma-encode`, `ozma-transport` |
| OpenAPI/JSON schema | Done | `docs/openapi.yaml` covers the node API, sessions, displays, transport, diagnostics, and errors |
| Orphaned legacy modules wired or deleted | Partly done | `src/` prototypes deleted; `ozma-node/src/tasks/udp.rs` replaced by `controller_compat`. `ozma-node/src/{capture,serve,hotplug}.rs` and the remaining `src/tasks/*` are still unreferenced and stay with the Phase 2 HLS/task wiring |

## M1 — One real media path

| Item | Status | Evidence / limit |
|---|---|---|
| V4L2 discovery | Done | `/dev` + sysfs; no C deps for discovery |
| V4L2 MMAP capture | Done | real single-plane MMAP worker; tested against a real MJPEG camera (1920x1080) |
| PipeWire video capture | Done | `ozma-pipewire` `PipeWireVideoSource` (BGRA/BGRx/RGBx/I420/NV12/YUYV, explicit MemFd buffer params) and `ozma-capture::PipeWireCaptureProvider`; real camera captured 1920x1080 YUYV through PipeWire with 0 dropped. `CaptureRegistry::system_with_pipewire()` keeps V4L2 registered. Portal ScreenCast node adapter point documented; portal consent flow not run |
| Encoder selection/probe | Done | hardware preference, software fallback, probe records |
| Real H.264 encode | Done | `ozma-encode` OpenH264 software backend: MJPEG/NV12/YUYV/RGB/BGRA → I420 → Annex-B, real probe, keyframes |
| Real-camera end-to-end | Validated | `/dev/video0` 1920×1080 MJPEG → VAAPI High-profile H.264 → QUIC fragments → client reassembled and decoded a 45 KB keyframe to 1920×1080 PPM |
| Native QUIC session | Done | real QUIC + rustls, ALPN `ozma/1`, control stream + datagrams |
| Scoped view/control grants | Done | HMAC session tokens, permission sets, view-only input rejection |
| Workstation consent | Done | consent policy + session state machine; denied consent blocks start |
| HID input sink | Done | gadget writes, 6-key limit, absolute/relative reports, release-all, fake/recording sinks |
| Audio path | Done | `ozma-pipewire` native capture/playback and `ozma-audio` Opus; node prefers PipeWire and falls back to ALSA/fake. `AudioReframer` re-blocks 1024-sample PipeWire buffers into 960-sample Opus frames; real PipeWire capture now encodes without errors. `OZMA_AUDIO_SOURCE=fake` provides a paced tone |
| Video fragmentation/reassembly | Done | protocol fragment header, node splitter, client `VideoAssembler` |
| Client camera/mic uplink | Done (browser + native) | `ChannelKind::Camera` + `CaptureStart/Stop/State` + `KeyframeRequest`; node decodes H.264/Opus into a bounded hub with `/api/v1/sessions/{id}/capture` and a PPM proof endpoint. Chromium/Firefox published 1280x720 camera + mic (node decoded both, PPM valid). Native `ozma-client`/`ozma-play` published a real `/dev/video0` (VAAPI/OpenH264, 1920x1080) and PipeWire microphone (499 packets, 958,080 samples); `--camera`/`--mic` flags |
| Input release on disconnect/control loss | Done | `InputReleased` + `ReleaseAll`; QUIC test checks the sink is cleared |
| Session offer with token | Done | `POST /api/v1/sessions` returns a QUIC offer when configured |
| Client video decode | Done | `ozma-decode`: OpenH264 H.264 decoder, I420/RGBA/PPM output, Opus decode; real 1920×1080 frame decoded and saved as PPM |
| Controller connect flow | Done | `ozma-client connect`: controller discovery/activation → node session create/consent/start → transport certificate → QUIC media probe |
| Client playback | Done | `ozma-playback`: PipeWire audio sink with ALSA fallback, null/PPM/window video sinks, bounded playback engine |
| Live playback CLI | Done | `ozma-play`: decode video/audio and present to PPM, null, or optional `minifb` window |
| Native client | Done | `ozma-client` controller/node clients, connect/probe CLI, fragment reassembly, optional decoded PPM frame; desktop rendering is external |
| Input latency measurement | Done (send → encoded frame) | `control.InputLatency`/ack and `ozma-client probe --input-latency`; decode/display/scanout are not included, so no <30 ms SLO claim |

## M2 — Remote monitor spike

| Item | Status | Evidence / limit |
|---|---|---|
| `VirtualDisplayProvider` + fake provider | Done | `ozma-display`, 44 tests |
| EDID builder | Done | 128-byte EDID 1.4 base block, checksum test |
| Topology + input mapping | Done | scale 100/125/150%, rotation 0/90/180/270 |
| API display create/list/destroy/mode/DPMS/topology | Done | backed by the fake provider; real provider gated |
| Xorg + modesetting + EVDI + libevdi spike | Documented, not run | `docs/remote-display-evdi-spike.md` |
| Real EVDI provider | Scaffold only | `EvdiProvider` detects prerequisites and refuses unsupported |
| Browser WebTransport | Done (Chromium + Firefox) | Node `wtransport` listener with ECDSA/14-day certificate pinning, session offers, and uni-stream resources. Headless Chromium ran the full path: CORS session create/start, hash-pinned WebTransport, Hello/HelloAck, live video, and the Rust/wasm viewer rendered 1920x1080 frames. A flipped certificate hash failed the handshake. Firefox/Chrome/Safari matrix and audio are not yet exercised |
| SSH/Unix fallback, passkeys | Not started | — |

## Remote scene outputs (development, loopback)

| Output | Status | Evidence / limit |
|---|---|---|
| RDP | Direct surface + auth validated | FreeRDP shadow subsystem writes OZFR frames into its own primary surface (no Xvfb/ffplay); TLS-only, token/user auth, viewer limit. Stock `xfreerdp3` verified frames and rejection paths. RDPEGFX/H.264 and scene flattening remain. |
| SPICE | Direct surfaces + auth validated | QXL secondary surfaces for image placements, damage rectangles, TLS-only ticket auth, PCM playback channel. `spicy`/screenshot tests verified movement, removal, damage, auth rejection, and playback channel init. One viewer; no SPICE video codec/A-V sync. |
| Xpra | Video path validated at wire level | Handshake fields, PNG delivery, draw acknowledgement, placement-only updates, codec negotiation, and encoded H.264/VP8/VP9/JPEG frames. Stock Xpra remains unverified; compressed hello records are not accepted. |
| Browser scene producer | Live bridge | WebSocket `/api/v1/browser/scene/ws` ingests browser-painted scenes; bounded resource/commit/video hub, `live.mjs` fixture bridge; browser video is metadata-only. This is a producer path, not the viewer. |
| Web viewer | Done (H.264, Chromium + Firefox) | Native Ozma client compiled to wasm32 (`ozma-web`): WebTransport session, shared `ozma-client-core` framing/`VideoAssembler`, WebCodecs H.264, canvas, Opus/Web Audio, pointer/keyboard (USB HID) input, and camera/mic uplink. Chromium and Firefox both rendered live video, decoded a paced Opus stream, published a 1280x720 fake camera (node decoded 117/78 frames) and 376,320 microphone samples, and measured a non-zero analyser level |
| VNC | Development output | RFB 3.8 framebuffer server with damage updates; viewer input discarded. |
| Google Cast | Development output | Live HLS composed output with local playback validation; physical receiver and controller integration remain open. |

These are **development outputs**, not the native Ozma session protocol. RDP and
SPICE currently receive composed pixels; direct graphics surfaces, viewer
authentication, and Xpra video are being extended in separate workstreams.

## Verification

```bash
cargo test --workspace                                   # 1205 passed, 13 ignored (current)
cargo test -p ozma-node --test parity                    # Python/controller parity baseline
cargo test -p ozma-node --lib                            # node API, compat, session unit tests (69)
cargo test -p ozma-qmp -p ozma-input -p ozma-power       # QMP, uinput/QMP sinks, power (166)
cargo test -p ozma-capture -p ozma-display -p ozma-audio # VM capture, display model, audio target
cargo test -p ozma-aperture                              # aperture meet/receipts/policy loader
cargo test -p ozma-node --test quic_session              # real QUIC end-to-end
cargo test -p ozma-node --test m0_acceptance
cargo test -p ozma-capture                               # opt-in real camera/v4l2loopback tests are ignored
OZMA_CAPTURE_TEST_DEVICE=/dev/video2 \
  cargo test -p ozma-capture -- --ignored real_v4l2_capture
OZMA_VNC_TEST=1 OZMA_VNC_HOST=127.0.0.1 OZMA_VNC_PORT=5901 \
  cargo test -p ozma-capture -- --ignored real_vnc_ffmpeg_captures_a_frame
OZMA_TEST_CAPTURE_DEVICE=/dev/video0 \
  cargo test -p ozma-client --lib -- --ignored real_camera_publishes_frames
```

## Deliberate limitations

- QMP display capture is a polling `screendump` path: every frame costs a QMP
  round trip plus a PPM write/read, and it captures QEMU's default console only.
  VNC capture is implemented only as an ffmpeg transition path: unauthenticated
  loopback VNC, every frame scaled to the configured size, no VNC audio/input,
  and no live VNC/ffmpeg run in CI. The D-Bus framebuffer provider is still not
  implemented.
- The uinput sink is unit-tested but its real `/dev/uinput` test is ignored; the
  host user is not in the `input` group. QMP sinks are tested against a fake
  server, not a real QEMU. QMP has no horizontal wheel, touch, or pen event, and
  those inputs are rejected rather than faked.
- `PowerAction::Stop` over QMP calls QEMU `stop` (pause), not destroy; a
  force-off falls back to ACPI powerdown. The libvirt backend uses `virsh`, not
  the libvirt API.
- Audio target selection builds `<sink>.monitor` names but does not create,
  route, or verify a PipeWire sink/monitor node. Per-VM routing remains open.
- HLS is a local rawvideo→ffmpeg pipeline and does not delete old segments.
  WebRTC is helper-first only: it supervises the external GStreamer/Python
  worker, has no native stack or audio, and the current worker does not
  acknowledge bitrate changes. No live helper or controller negotiation has run
  in CI. D-Bus framebuffer capture, live VNC validation, runtime codec override,
  audio routing, and full `ozma-softnode` provisioning remain unimplemented.
  Gate 2 is not met until live validation and those remaining pieces land.
- `ozma-softnode` supervises `ozma-node --profile vm` subprocesses and allocates
  ports, but it does not create QMP/display sockets, edit libvirt XML, provision
  evdev/PulseAudio devices, or aggregate status/metrics yet.
- Video uses VAAPI when its real probe passes and OpenH264 otherwise.
  NVENC/QSV/V4L2M2M are not implemented; the selection policy refuses to
  pretend a software result is hardware.
- VAAPI forced keyframes restart the FFmpeg sidecar; periodic GOP works
  without a restart.
- QUIC carries control/input/video/audio over a real connection. A second
  WebTransport (HTTP/3) listener carries the same framing for browsers; it is
  covered by a native `wtransport` proof client and by a real headless Chromium
  run and by the wasm viewer itself: headless Chromium connected through
  WebTransport and rendered live 1920x1080 H.264 frames on a canvas; Firefox
  159 rendered live 320x240 frames and both decoded a paced Opus stream. Wrong
  certificate hashes are rejected. Client camera/microphone uplink is being
  added and verified in Chromium/Firefox with fake devices: the node decoded
  camera H.264 and microphone Opus, returned a valid PPM frame, and the
  analyser reported non-silent audio. The native client can publish its own
  camera and microphone with `--camera`/`--mic`. A/V drift is measured but not
  corrected; Firefox loses whole access units when a camera fragment is
  dropped; USB UVC/UAC2 injection, HEVC/AV1/AAC, and Safari remain open.
- Native microphone capture requires a session created with `audio.mic = true`;
  `ozma-play` cannot add the channel to an existing offer.
- PipeWire video capture is opt-in through `CaptureRegistry::system_with_pipewire()`;
  `system()` stays V4L2-only. Portal ScreenCast selection is an adapter point,
  not an implemented consent flow.
- The node RTSP listener broadcasts one default capture/encode pipeline to all
  RTSP viewers and seeds new viewers with the latest keyframe.
- The virtual-display API is exercised through the fake provider. The EVDI
  provider has not been validated on a real Xorg/modesetting session.
- The HTTP API supports an optional static bearer token (`--api-token` /
  `OZMA_API_TOKEN`). Controller-issued short-lived HTTP credentials and
  passkey pairing are still future work.
- The native client library, headless probe, and optional `ozma-play` window
  are done. The window is basic and feature-gated; full desktop UX remains an
  external client concern.
- Audio is PipeWire-native on Linux with an ALSA fallback; fixed 48 kHz stereo
  S16, no A/V sync, jitter buffer, or resampling yet.
- The real-camera test used the USB webcam on this workstation; hardware
  encoder paths, 4K, HDR, and long-run thermal behavior are not validated.
- The HID gadget is unavailable on this host, so input was validated through
  the recording sink rather than a real USB target. When `/dev/hidg0` and
  `/dev/hidg1` cannot be opened, the node uses the fake sink, reports
  `sink_kind: "fake-recording"` on `/api/v1/connection`, and does **not**
  claim to inject input.
- The controller-compat UDP HID listener, `/api/v1/connection`, HLS
  `/stream/*`, `/input/*`, `/power/*`, `/codec`, and helper-first `/webrtc/*`
  are implemented. Audio routing endpoints intentionally answer typed 501s.
- `ozma-node --profile vm` wires QMP capture/input/power when the flags are
  present; `--profile desktop` still records provider selections and does not
  start uinput/PipeWire providers automatically.
- `ozma-softnode` is discovery/status/agent-provisioning only. It no longer
  spawns a nonexistent child binary; auto-management fails fast and points at
  `ozma-node --profile vm`.
- `ozma-node` still carries unreferenced legacy capture/HLS/task modules from
  the earlier sketch (`capture.rs`, `serve.rs`, `hotplug.rs`, and most of
  `src/tasks/*`); they are not part of the build and remain Phase 2 wiring.
- `v4l` pulls `v4l2-sys-mit`, which needs libclang at build time.
