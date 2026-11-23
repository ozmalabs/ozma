# Plan execution status

Tracking the implementation of [`node-hermes-parity.md`](node-hermes-parity.md).
This file records what is actually merged and tested, not what is planned.

## M0 — Contract and cleanup

| Item | Status | Evidence |
|---|---|---|
| Shared `ozma-protocol` crate | Done | capabilities, sessions, permissions, transports, displays, input, control, diagnostics, errors; 19 tests |
| Node builds without native OpenSSL | Done | `ozma-node` uses `reqwest` + `rustls-tls` |
| Versioned node API skeleton | Done | `/api/v1/health`, `/node`, `/capabilities`, `/diagnostics`, `/metrics`, `/sessions`, `/displays` |
| Fake session reaches `Ready`/`Streaming` | Done | `cargo test -p ozma-node --test m0_acceptance` |
| Fake capture/encoder/transport | Done | `ozma-media`, `ozma-capture`, `ozma-encode`, `ozma-transport`; 37+ tests |
| OpenAPI/JSON schema | Not started | — |
| Orphaned legacy modules wired or deleted | Not started | `ozma-node/src/{capture,serve,hotplug}.rs`, `src/tasks/*` remain unreferenced |

## M1 — One real media path

| Item | Status | Evidence / limit |
|---|---|---|
| V4L2 discovery | Done | `ozma-capture` reads `/dev` + sysfs, no C deps |
| V4L2 MMAP capture | Not started | `V4l2Capture` honestly returns `capture.unavailable` |
| Encoder selection/probe | Done for selection | `ozma-encode` hardware preference, software fallback, probe records |
| Real H.264 encode | Not started | fake encoder is the tested path |
| Native QUIC session | Done | real QUIC with rustls, ALPN `ozma/1`, control stream + datagrams |
| Scoped view/control grants | Done | HMAC session tokens, permission sets, view-only rejection |
| Workstation consent | Done | consent policy + session state machine; denied consent blocks start |
| Input release on disconnect/control loss | Done | `InputReleased` + `ReleaseAll`; QUIC disconnect test |
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

## Verification commands

```bash
cargo test --workspace          # currently green
cargo test -p ozma-node --test quic_session
cargo test -p ozma-node --test m0_acceptance
cargo test -p ozma-display
```

## Deliberate limitations

- The media path is the deterministic fake pipeline; real V4L2/MMAP and
  hardware/software H.264 encoding remain M1 work.
- QUIC carries control/input/video over a real connection, but there is no
  browser WebTransport gateway yet.
- The virtual-display API is exercised through the fake provider. The EVDI
  provider has not been validated on a real Xorg/modesetting session.
- The HTTP API is a local/trusted surface; the QUIC session path is the first
  one to enforce controller-issued tokens and consent. HTTP bearer auth is next.
- `ozma-node` still carries unreferenced legacy capture/HLS/task modules from
  the earlier sketch; they are not part of the build.
