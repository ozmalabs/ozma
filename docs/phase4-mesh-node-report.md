# Phase 4 — Mesh service in the Rust node (report)

## Scope

This change wires the existing `ozma-mesh` crate into `ozma-node` as an
**opt-in** node-local mesh service and exposes it through the controller:

* `ozma-mesh`: a small peer-status API (`MeshManager::peer_statuses`), an honest
  `MeshManager::stop`, and `MeshManager` is now `Sync` (the libmdns responder
  handles live behind a `Mutex`) so node async traits can call it. It also owns
  a typed, injectable Linux dataplane (TUN link, routes, IPv4 forwarding,
  opt-in NAT, and a bounded TUN/WireGuard packet pump) in
  [`dataplane`](phase4-mesh-dataplane-report.md), plus opt-in mDNS peer
  discovery and route reconciliation in
  [`phase4-mesh-discovery-report.md`](phase4-mesh-discovery-report.md).
* `ozma-node`: new `mesh_service` module, `--mesh*` CLI/env flags, guarded
  `/api/v1/connect/{status,peers,events}` routes, startup/shutdown wiring, and
  optional `--mesh-dataplane` lifecycle wiring.
* `controller/api.py`: node-scoped mesh/Connect passthrough routes that send the
  node bearer token and preserve typed `supported:false` responses.
* Tests: in-memory fake mesh backend, fake dataplane command/sysfs seams, node
  route tests, and controller proxy tests.

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were intentionally
not edited.

## Default behavior is unchanged

Mesh is off unless `--mesh` / `OZMA_MESH` is supplied. Without it:

* no UDP socket is bound and no mDNS service is registered;
* `build_mesh_service` returns `None`;
* `GET /api/v1/connect/status` returns HTTP 200 with
  `state:"not-configured"`, `available:false`, and **no** peer count or peer
  list.

The kernel dataplane is a second explicit opt-in (`--mesh-dataplane`). Without
it no TUN device is created, no route/forwarding/NAT state is touched, and the
status document has no `dataplane` field.

A Python soft node that does not serve these routes keeps its previous behavior;
the controller only adds new node-scoped routes and returns the node's plain
404 for them.

## CLI / environment

| Flag | Env | Meaning |
|---|---|---|
| `--mesh` | `OZMA_MESH` | Opt in to the node mesh service. |
| `--mesh-ip` | `OZMA_MESH_IP` | IPv4 address assigned to this node on the mesh. |
| `--mesh-port` | `OZMA_MESH_PORT` | WireGuard UDP port (default `51820`, must be non-zero). |
| `--mesh-key-file` | `OZMA_MESH_KEY_FILE` | Base64 X25519 WireGuard private-key file. |
| `--mesh-kernel` | `OZMA_MESH_KERNEL` | Request kernel WireGuard (root + `wg`). Kernel mode is **not implemented** and reports a typed unsupported state. |
| `--mesh-peer` | `OZMA_MESH_PEER` | Repeatable JSON `MeshNode` peer object. Static and authoritative. |
| `--mesh-discover` | `OZMA_MESH_DISCOVER` | Opt in to mDNS browsing on `_ozma-mesh._udp.local`. Requires `--mesh`. |
| `--mesh-discover-ttl` | `OZMA_MESH_DISCOVER_TTL` | Seconds an absent discovered peer is retained (default `60`). |
| `--mesh-discover-max-peers` | `OZMA_MESH_DISCOVER_MAX_PEERS` | Discovered-peer bound (default `32`, hard limit `256`). |
| `--mesh-trust` | `OZMA_MESH_TRUST` | Repeatable discovery authorization: JSON `TrustedPeer` or `ID=BASE64_KEY`. |
| `--mesh-dataplane` | `OZMA_MESH_DATAPLANE` | Opt in to the kernel TUN/routing/forwarding dataplane. Requires root, `/dev/net/tun`, and `ip`. |
| `--mesh-tun` | `OZMA_MESH_TUN` | TUN interface name (default `ozma0`). |
| `--mesh-mtu` | `OZMA_MESH_MTU` | TUN MTU (default `1420`). |
| `--mesh-prefix-len` | `OZMA_MESH_PREFIX_LEN` | Mesh prefix length (default `24`). |
| `--mesh-dataplane-nat` | `OZMA_MESH_DATAPLANE_NAT` | Explicitly opt in to nftables masquerade (requires `nft`). |
| `--mesh-nat-out` | `OZMA_MESH_NAT_OUT` | NAT egress interface; without it, non-mesh egress is masqueraded. |

The private key is only read from the key file; it is never serialized, logged,
or included in any status document. `MeshIdentity` contains the public key only.

## Service model

`ozma_node::mesh_service::MeshService` owns a backend plus a bounded event log.

States: `not-configured`, `stopped`, `starting`, `running`, `unsupported`,
`failed`.

Peer/tunnel model:

* Adding a peer creates an **idle** tunnel entry, not a connected one.
* A peer is `established` only when boringtun reports a completed handshake
  (`Tunn::stats()` time-since-handshake is `Some`).
* Counter fields (`tx_bytes`, `rx_bytes`, `loss`, `rtt_ms`, handshake age) are
  boringtun's real values; a newly added peer reports zeroes and idle.
* Static peers report `origin:"static"`; discovered peers report
  `origin:"discovered"`, their advertised capabilities, and the actual
  `endpoint` used for WireGuard UDP transport.
* `peer_count` and `peers` are omitted unless the service is running. A running
  service with no peers reports a real `peer_count: 0` and `peers: []`.

Event log:

* Capacity is bounded (`MESH_EVENT_CAPACITY = 64`, or a smaller configured
  value); oldest events are evicted.
* Events cover start/stop, peer add/remove, tunnel-state transitions, and
  typed errors, with monotonic `seq` and Unix-millisecond timestamps.
* `GET /api/v1/connect/events?limit=N` clamps `N` to the capacity.

### Dataplane lifecycle (opt-in)

When `--mesh-dataplane` is set, `MeshService` starts the dataplane **before**
the WireGuard manager, so a missing TUN device, root, or `ip` never leaves a
WireGuard socket bound. A manager start failure tears the dataplane back down.
After both are running, the service opens the persistent TUN fd and starts the
bounded packet pump. `state:"running"` and `available:true` require the
WireGuard manager, the dataplane, and the configured pump to all be running; a
failed or still-starting dataplane/pump is reported as `failed`/`starting`,
never as `running`.

The dataplane status object is separate from peer handshake state:

```json
"dataplane": {
  "configured": true,
  "state": "running",
  "interface": "ozma0",
  "address": "10.200.9.1/24",
  "mtu": 1420,
  "up": true,
  "forwarding": true,
  "routes": ["10.200.9.2/32"],
  "nat": false
}
```

A failure adds a typed `last_error` (`code`/`reason`). Adding a peer and
programming its route never marks that peer `established`; only a real
boringtun handshake does. See
[`phase4-mesh-dataplane-report.md`](phase4-mesh-dataplane-report.md).

### Discovery lifecycle (opt-in)

With `--mesh-discover`, the service browses `_ozma-mesh._udp.local` every five
seconds. An advertisement is only accepted when its node id + WireGuard public
key match a static `--mesh-peer` entry or a `--mesh-trust` entry. Static peers
are authoritative, so a conflicting advertisement is reported and ignored.
Accepted peers are added to the backend, get a `/32` route through the existing
dataplane seam, and have a WireGuard handshake initiated. When a peer is not
seen for `--mesh-discover-ttl` seconds its tunnel and route are removed. A
changed mesh IP replaces the one tunnel for that id (no duplicate tunnel), adds
the new route, and removes the old one.

Discovered peers use the observed mDNS underlay address as their WireGuard UDP
endpoint while keeping their mesh IP as the overlay identity. This keeps
encrypted UDP off the mesh TUN and avoids the static-peer endpoint recursion
noted in the live reports.

Discovery status appears next to the dataplane/pump documents:

```json
"discovery": {
  "enabled": true,
  "local_id": "ozma-disc-a",
  "max_peers": 8,
  "ttl_secs": 10,
  "static_peer_count": 0,
  "trusted_key_count": 1,
  "discovered_count": 1,
  "peers": [{"id":"ozma-disc-b","mesh_ip":"10.200.1.2","wg_port":51820,
             "wg_pubkey":"...","capabilities":["audio","video"],
             "underlay_ip":"10.10.20.1","age_secs":0,"expires_in_secs":9}],
  "counters": {"advertisements":4,"authorized":2,"added":1,"updated":0,
               "removed":0,"duplicates":1,"expired":0,"conflicts":0,
               "unauthorized":0,"invalid":0,"limit_dropped":0,
               "self_ignored":2,"static_matched":0}
}
```

Discovery events use the bounded event log: `peer-discovered`, `peer-removed`,
`route`, and `conflict` (refusals), plus `error` for programming failures.

Typed prerequisites (`MeshServiceError.code`):

| Code | Meaning |
|---|---|
| `mesh.not-configured` | Missing identity/IP/port. |
| `mesh.ip-invalid` | `--mesh-ip` is not IPv4. |
| `mesh.key-missing` | No readable key file. |
| `mesh.key-invalid` | Key file is not valid base64 X25519 material. |
| `mesh.root-required` | Kernel mode requested, node is not root. |
| `mesh.wg-missing` | Kernel mode requested, `wg` is not on `PATH`. |
| `mesh.wireguard-unavailable` | No usable userspace or kernel backend. |
| `mesh.kernel-unsupported` | Root/`wg` are present but kernel mode is not implemented. |
| `mesh.peer-invalid` | Configured peer JSON/address is invalid. |
| `mesh.discovery-invalid` | Discovery bounds or a `--mesh-trust` entry is invalid. |
| `mesh.peer-conflict` | A discovered advertisement conflicts with a static/other peer. |
| `mesh.peer-unauthorized` | An advertisement lacks a matching key/host authorization. |
| `mesh.peer-already-exists` / `mesh.peer-not-found` | Typed peer-table errors. |
| `mesh.dataplane-unavailable` | `/dev/net/tun`, `ip`, or (for NAT) `nft` is unavailable. |
| `mesh.dataplane-invalid` | TUN name, address/prefix, MTU, peer route, or NAT table/interface is invalid. |
| `mesh.dataplane-not-configured` | A dataplane operation was attempted with no configuration. |
| `mesh.dataplane-command-failed` | `ip`/`nft` exited non-zero; the applied steps are rolled back. |
| `mesh.tun-open` | The persistent TUN fd could not be attached for the packet pump. |
| `mesh.pump-*` | Typed packet-pump read/write/tunnel/queue failure; the pump stops and reports `failed`. |
| `mesh.start-failed` | Runtime bind/mDNS/start failure. |
| `mesh.not-running` | Peer operation attempted while stopped. |

Prerequisite codes are returned as typed HTTP 501 documents; runtime failures
are reported in the status document (`state:"failed"`, `code`, `reason`) and do
not fabricate peers.

## Node routes

All three routes are merged into `controller_compat::compat_router` **before**
the node API bearer layer, so they use the same `--api-token` guard as
`/input/*`, `/power/*`, `/codec`, and `/usb`.

* `GET /api/v1/connect/status`
* `GET /api/v1/connect/peers`
* `GET /api/v1/connect/events?limit=N`

Status includes:

* `capability:"mesh"`, `capabilities:["/api/v1/connect/status",
  "/api/v1/connect/peers","/api/v1/connect/events"]` (discoverable even while
  disabled);
* `enabled`, `state`, `available`, `wireguard` (`userspace-boringtun`,
  `not-configured`, or `unavailable`);
* optional `identity`, `dataplane`, `pump`, `discovery`, `peer_count`, `peers`,
  `event_count`, `event_capacity`;
* typed `code`/`reason` when unavailable.

Typed 501 body:

```json
{
  "ok": false,
  "code": "mesh.key-missing",
  "surface": "/api/v1/connect/status",
  "capability": "mesh",
  "supported": false,
  "reason": "...",
  "error": "...",
  "remediation": "... never fabricates peers, tunnels, or handshake state."
}
```

## Controller passthrough

New controller routes:

* `GET /api/v1/nodes/{node_id}/connect/status`
* `GET /api/v1/nodes/{node_id}/connect/peers`
* `GET /api/v1/nodes/{node_id}/connect/events`

`_proxy_node_connect` reuses the existing node proxy and:

* sends the per-node token, then the controller-wide `OZMA_NODE_API_TOKEN`
  fallback, via `_node_headers`;
* preserves a typed `supported:false` 501 body unchanged;
* maps a node 401/403 to a token-free 502;
* returns a plain 404 for a Python soft node that does not serve the route.

The existing global `/api/v1/connect/status` cloud client is not replaced or
proxied; a test asserts it still returns the Connect account/tier document.

## Tests

Run for this change:

* `cargo test -p ozma-mesh -p ozma-node --lib` — **611 passed, 6 ignored**
  (`ozma-mesh`: 48 passed, 2 ignored; `ozma-node`: 563 passed, 4 ignored).
* `cargo test -p ozma-node --bin ozma-node mesh_` — **2 passed** (CLI defaults
  and trust parsing).
* `cargo check -p ozma-mesh -p ozma-node --all-targets` — passes (pre-existing
  warnings only).
* `rustfmt --edition 2021 --check` on every touched Rust file — passes.

Coverage includes:

* fake backend start/stop, peer add/remove/duplicate, tunnel state transitions,
  bounded events, key/root/`wg` typed errors, unsupported status, failed start;
* fake dataplane command/sysfs seams: exact argv/order, forward/rollback,
  idempotent stop, NAT opt-in, route add/remove, root/TUN/tool typed errors,
  address/MTU/interface validation, and a `starting` status seen mid-command;
* node routes for not-configured (no counts/peers), typed 501, running with real
  `peer_count:0`/`peers:[]`, and peers/events after a real add;
* node dataplane status for stopped/starting/running/failed, no fake `running`,
  and peer add/remove programming routes without claiming a handshake;
* packet-pump status absent without a dataplane, stopped before start,
  starting while the TUN opens, running with real zero counters, and failed
  with typed `mesh.tun-open`/`mesh.pump-*` errors that roll back the runtime;
* fake discovery stream: authorized add, untrusted key rejected before any
  peer/route/handshake, duplicate dedupe, TTL removal, mesh-IP change with one
  tunnel and two route changes, static-vs-discovered conflict, max-peer bound,
  malformed records, and mDNS wire parsing/assembly;
* manager origin safety: invalid advertised key installs nothing, discovered
  endpoint updates replace the tunnel, static peers cannot be overwritten or
  removed by discovery, and two ids cannot share a mesh IP;
* node discovery status/events and restart without stale deduplication;
* controller token forwarding, status/capability discovery, typed 501
  passthrough, Python-node 404, auth-failure mapping, and unchanged global
  Connect status.

No default test requires root, a real WireGuard interface, a real TUN device, a
network namespace, or a real peer. The live dataplane and live TUN-attach tests
are `#[ignore]`d and additionally gated by `OZMA_MESH_LIVE=1`.

## Honest remaining gaps (modelled or unwired)

* **Kernel WireGuard is not implemented.** `--mesh-kernel` only produces typed
  `mesh.root-required`, `mesh.wg-missing`, or `mesh.kernel-unsupported`; all
  traffic uses userspace boringtun.
* **No root/wg requirement for the default WireGuard backend.** Userspace
  boringtun needs neither. Root/TUN/`ip` are required only when
  `--mesh-dataplane` is explicitly enabled; NAT additionally requires `nft`.
* **mDNS advertisements are unauthenticated.** Discovery is opt-in and every
  advertisement must match a static peer or a `--mesh-trust` key, but mDNS
  itself provides no signature or replay protection. Trust entries are the
  authorization boundary; there is no controller-issued mesh CA yet.
* **No automatic handshake/peer promotion for static peers.** Discovered peers
  get an initiated handshake; a static peer still becomes established through
  its own traffic or an external call. A real established state always
  requires a boringtun handshake, never configuration.
* **Packet-pump live-network coverage is partial.** The pump now owns the TUN
  fd and moves IPv4 plaintext to/from boringtun, but default tests use
  in-memory seams. There is no live multi-node encrypted packet-flow test, and
  the pump is single-queue IPv4-only with no PMTU/ICMP handling. See
  [`phase4-mesh-dataplane-report.md`](phase4-mesh-dataplane-report.md).
* **No key generation/persistence/rotation.** The operator supplies a key file;
  the node neither creates one nor persists a generated identity.
* **No controller cloud relay integration.** This wires node-local mesh state
  only. Ozma Connect relay registration, relay mesh IP assignment, controller
  mesh CA pairing, and cloud failover remain Python-side/cloud concerns.
* **No live controller ↔ Rust-node mesh test.** Controller tests use a fake HTTP
  node; no real node process, WireGuard handshake, or `/api/v1/connect/*` call
  was exercised over the network in this change.
