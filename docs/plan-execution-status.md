# Plan execution status

Tracking the implementation of [`node-hermes-parity.md`](node-hermes-parity.md).
This file records what is actually merged and tested, not what is planned.

## M0 — Contract and cleanup

| Item | Status | Evidence |
|---|---|---|
| Shared `ozma-protocol` crate | Done | capabilities, sessions, permissions, transports, displays, input, control, diagnostics, errors; 19 tests |
| Node builds without native OpenSSL | Done | `ozma-node` uses `reqwest` + `rustls-tls` |
| Versioned node API | Done | `/api/v1/health`, `/node`, `/capabilities`, `/diagnostics`, `/metrics`, `/sessions`, `/displays`, Hestia capability discovery |
| Fake session reaches `Ready`/`Streaming` | Done | `cargo test -p ozma-node --test m0_acceptance` |
| Fake capture/encoder/transport | Done | `ozma-media`, `ozma-capture`, `ozma-encode`, `ozma-transport` |
| OpenAPI/JSON schema | Done | `docs/openapi.yaml` covers the node API, sessions, displays, transport, diagnostics, and errors |
| Orphaned legacy modules wired or deleted | Not started | `ozma-node/src/{capture,serve,hotplug}.rs`, `src/tasks/*` remain unreferenced |

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
| Audio path | Done | `ozma-pipewire` native capture/playback and `ozma-audio` Opus; node prefers PipeWire and falls back to ALSA/fake; real PipeWire daemon smoke test passed |
| Video fragmentation/reassembly | Done | protocol fragment header, node splitter, client `VideoAssembler` |
| Client camera/mic uplink | In progress | `ChannelKind::Camera` + `CaptureStart/Stop/State` + `KeyframeRequest`; node decodes H.264/Opus into a bounded per-session hub with `/api/v1/sessions/{id}/capture` and a PPM proof endpoint; `RunningPipeline::request_keyframe` is real. Browser (`getUserMedia` + WebCodecs) and native (`ozma-capture`/`ozma-encode`/Opus) uplink clients are being implemented |
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
| Web viewer | Done (H.264, Chromium + Firefox) | Native Ozma client compiled to wasm32 (`ozma-web`): WebTransport session, shared `ozma-client-core` framing/`VideoAssembler`, WebCodecs H.264, canvas, Opus/Web Audio, pointer/keyboard (USB HID) input. Headless Chromium rendered live 1920x1080 video and Firefox 159 rendered live 320x240; both decode a paced Opus stream with no errors |
| VNC | Development output | RFB 3.8 framebuffer server with damage updates; viewer input discarded. |
| Google Cast | Development output | Live HLS composed output with local playback validation; physical receiver and controller integration remain open. |

These are **development outputs**, not the native Ozma session protocol. RDP and
SPICE currently receive composed pixels; direct graphics surfaces, viewer
authentication, and Xpra video are being extended in separate workstreams.

## Verification

```bash
cargo test --workspace                                   # green
cargo test -p ozma-node --test quic_session              # real QUIC end-to-end
cargo test -p ozma-node --test m0_acceptance
cargo test -p ozma-capture                               # opt-in real camera test is ignored
OZMA_CAPTURE_TEST_DEVICE=/dev/video2 \
  cargo test -p ozma-capture -- --ignored real_v4l2_capture
```

## Deliberate limitations

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
  added; audible-output level, lip sync, HEVC/AV1/AAC, and Safari remain open.
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
  the recording sink rather than a real USB target.
- `ozma-node` still carries unreferenced legacy capture/HLS/task modules from
  the earlier sketch; they are not part of the build.
- `v4l` pulls `v4l2-sys-mit`, which needs libclang at build time.
