# Plan execution status

Tracking the implementation of [`node-hermes-parity.md`](node-hermes-parity.md).
This file records what is actually merged and tested, not what is planned.

## M0 — Contract and cleanup

| Item | Status | Evidence |
|---|---|---|
| Shared `ozma-protocol` crate | Done | `cargo test -p ozma-protocol` — 18 tests |
| Capabilities, session states, errors, transports, displays | Done | `ozma-protocol/src/*` |
| Node builds without native OpenSSL | Done | `ozma-node` now uses `reqwest` + `rustls-tls` |
| Versioned node API skeleton | Done | `/api/v1/health`, `/node`, `/capabilities`, `/diagnostics`, `/metrics`, `/sessions`, `/displays` |
| Fake session reaches `Streaming` | Done | `cargo test -p ozma-node --test m0_acceptance` |
| Fake capture/encoder/transport | In progress | `ozma-media`, `ozma-capture`, `ozma-encode`, `ozma-transport` workstreams |
| OpenAPI/JSON schema | Not started | — |
| Orphaned legacy modules wired or deleted | Not started | `ozma-node/src/{capture,serve,hotplug}.rs`, `src/tasks/*` remain unreferenced |

## M1 — One real media path

| Item | Status |
|---|---|
| V4L2 capture with MMAP/hotplug | In progress (`ozma-capture` workstream) |
| Real frame probe + H.264/software fallback | In progress (`ozma-media`, `ozma-encode` workstreams) |
| Native QUIC session | In progress (`ozma-transport` workstream) |
| Scoped view/control grants + workstation consent | In progress (`ozma-auth`, `ozma-session` workstreams) |
| Input release on disconnect/control loss | In progress (`ozma-session` workstream) |
| Native desktop client end-to-end | Not started |
| Input latency measurement (<30 ms target) | Not started |

## M2 — Remote monitor spike

| Item | Status |
|---|---|
| `VirtualDisplayProvider` + fake provider + topology/input mapping + EDID | In progress (`ozma-display` workstream) |
| Xorg + modesetting + EVDI + libevdi spike | In progress (`docs/remote-display-evdi-spike.md` expected) |
| Browser WebTransport, SSH/Unix fallback, passkeys | Not started |

## Verification commands

```bash
cargo test -p ozma-protocol
cargo test -p ozma-api
cargo test -p ozma-node
cargo test --workspace
```
