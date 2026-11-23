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
| Real H.264 encode | Not started | QUIC currently feeds real V4L2 frames through the fake encoder; real FFmpeg/VAAPI backend is next |
| Native QUIC session | Done | real QUIC + rustls, ALPN `ozma/1`, control stream + datagrams |
| Scoped view/control grants | Done | HMAC session tokens, permission sets, view-only input rejection |
| Workstation consent | Done | consent policy + session state machine; denied consent blocks start |
| HID input sink | Done | gadget writes, 6-key limit, absolute/relative reports, release-all, fake/recording sinks |
| Input release on disconnect/control loss | Done | `InputReleased` + `ReleaseAll`; QUIC test checks the sink is cleared |
| Session offer with token | Done | `POST /api/v1/sessions` returns a QUIC offer when configured |
| Native desktop client end-to-end | Not started | server-side QUIC contract is tested with a Rust client |
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

- The QUIC media path can use a real V4L2 source, but the encoder is still the
  deterministic fake H.264 encoder. A real FFmpeg/VAAPI/software encoder is the
  next M1 item.
- QUIC carries control/input/video over a real connection, but there is no
  browser WebTransport gateway yet.
- The virtual-display API is exercised through the fake provider. The EVDI
  provider has not been validated on a real Xorg/modesetting session.
- The HTTP API is a local/trusted surface; the QUIC session path is the first
  one to enforce controller-issued tokens and consent. HTTP bearer auth is next.
- `ozma-node` still carries unreferenced legacy capture/HLS/task modules from
  the earlier sketch; they are not part of the build.
- `v4l` pulls `v4l2-sys-mit`, which needs libclang at build time.
