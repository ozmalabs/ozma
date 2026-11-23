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
| OpenAPI/JSON schema | Not started | — |
| Orphaned legacy modules wired or deleted | Not started | `ozma-node/src/{capture,serve,hotplug}.rs`, `src/tasks/*` remain unreferenced |

## M1 — One real media path

| Item | Status | Evidence / limit |
|---|---|---|
| V4L2 discovery | Done | `/dev` + sysfs; no C deps for discovery |
| V4L2 MMAP capture | Done | real single-plane MMAP worker; tested against a real MJPEG camera (1920x1080) |
| Encoder selection/probe | Done | hardware preference, software fallback, probe records |
| Real H.264 encode | Done | `ozma-encode` OpenH264 software backend: MJPEG/NV12/YUYV/RGB/BGRA → I420 → Annex-B, real probe, keyframes |
| Real-camera end-to-end | Validated | `/dev/video0` 1920×1080 MJPEG → VAAPI High-profile H.264 → QUIC fragments → client reassembled and decoded a 45 KB keyframe to 1920×1080 PPM |
| Native QUIC session | Done | real QUIC + rustls, ALPN `ozma/1`, control stream + datagrams |
| Scoped view/control grants | Done | HMAC session tokens, permission sets, view-only input rejection |
| Workstation consent | Done | consent policy + session state machine; denied consent blocks start |
| HID input sink | Done | gadget writes, 6-key limit, absolute/relative reports, release-all, fake/recording sinks |
| Audio path | Done | `ozma-pipewire` native capture/playback and `ozma-audio` Opus; node prefers PipeWire and falls back to ALSA/fake; real PipeWire daemon smoke test passed |
| Video fragmentation/reassembly | Done | protocol fragment header, node splitter, client `VideoAssembler` |
| Input release on disconnect/control loss | Done | `InputReleased` + `ReleaseAll`; QUIC test checks the sink is cleared |
| Session offer with token | Done | `POST /api/v1/sessions` returns a QUIC offer when configured |
| Client video decode | Done | `ozma-decode`: OpenH264 H.264 decoder, I420/RGBA/PPM output, Opus decode; real 1920×1080 frame decoded and saved as PPM |
| Controller connect flow | Done | `ozma-client connect`: controller discovery/activation → node session create/consent/start → transport certificate → QUIC media probe |
| Client playback | Done | `ozma-playback`: PipeWire audio sink with ALSA fallback, null/PPM/window video sinks, bounded playback engine |
| Live playback CLI | Done | `ozma-play`: decode video/audio and present to PPM, null, or optional `minifb` window |
| Native client | Done | `ozma-client` controller/node clients, connect/probe CLI, fragment reassembly, optional decoded PPM frame; desktop rendering is external |
| Input latency measurement (<30 ms) | Not started | — |

## M2 — Remote monitor spike

| Item | Status | Evidence / limit |
|---|---|---|
| `VirtualDisplayProvider` + fake provider | Done | `ozma-display`, 44 tests |
| EDID builder | Done | 128-byte EDID 1.4 base block, checksum test |
| Topology + input mapping | Done | scale 100/125/150%, rotation 0/90/180/270 |
| API display create/list/destroy/mode/DPMS/topology | Done | backed by the fake provider; real provider gated |
| Xorg + modesetting + EVDI + libevdi spike | Documented, not run | `docs/remote-display-evdi-spike.md` |
| Real EVDI provider | Scaffold only | `EvdiProvider` detects prerequisites and refuses unsupported |
| Browser WebTransport, SSH/Unix fallback, passkeys | Not started | QUIC is native raw QUIC, not WebTransport |

## Remote scene outputs (development, loopback)

| Output | Status | Evidence / limit |
|---|---|---|
| RDP | Loopback prototype validated | FreeRDP shadow on an isolated Xvfb display; stock FreeRDP viewer showed the Ozma overlay. Direct graphics surfaces and managed viewer auth are active work. |
| SPICE | Loopback prototype validated | libspice-server QXL interface; stock SPICE screenshot client captured the overlay and successive changes. Structured drawing and managed auth are active work. |
| Xpra | Wire-level validated | Handshake, original PNG delivery, draw acknowledgement, and placement-only update. Video and stock-client interoperability are active work. |
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
- QUIC carries control/input/video/audio over a real connection, but there is
  no browser WebTransport gateway yet.
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
