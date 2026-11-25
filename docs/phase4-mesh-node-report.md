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
  `/api/v1/connect/{status,peers,events}` routes, startup/shutdown wiring,
  optional `--mesh-dataplane` lifecycle wiring, and a selectable kernel
  WireGuard backend behind `--mesh-kernel`.
* `ozma-mesh`: a real Linux kernel WireGuard backend (`KernelWireguard`) with
  exact `ip`/`wg` argv, redacted key handling, `wg show dump` status parsing,
  peer add/remove, rollback, and typed root/module/tool failures. See
  [`phase4-mesh-kernel-report.md`](phase4-mesh-kernel-report.md).
* `ozma-mesh` / `ozma-node`: WireGuard key rotation with a bounded grace
  window, peer/link updates for every static and discovered peer, rollback on
  failure, `POST /api/v1/connect/rotate`, rotation status, and optional
  scheduled rotation. See
  [`phase4-mesh-rotation-report.md`](phase4-mesh-rotation-report.md).
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
| `--mesh-kernel` | `OZMA_MESH_KERNEL` | Select the Linux kernel `wireguard` backend. Requires root, the `wireguard` module, `ip`, and `wg`; the `--mesh-tun` name becomes the kernel link and no TUN/pump is created. |
| `--mesh-peer` | `OZMA_MESH_PEER` | Repeatable JSON `MeshNode` peer object. Static and authoritative. |
| `--mesh-peer-endpoint` | `OZMA_MESH_PEER_ENDPOINT` | Repeatable `ID=ADDRESS:PORT` underlay endpoint for a static peer. The peer's mesh IP remains its overlay identity and `/32` route. Useful in kernel mode when that mesh IP routes through the WireGuard link. |
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
| `--mesh-rotate-hours` | `OZMA_MESH_ROTATE_HOURS` | Scheduled key rotation interval. `0` (default) disables it; a non-zero value must be `1..=720` hours. |
| `--mesh-rotate-jitter-secs` | `OZMA_MESH_ROTATE_JITTER_SECS` | Random jitter added to each scheduled rotation (default `300`, max `3600`). |
| `--mesh-rotate-grace-secs` | `OZMA_MESH_ROTATE_GRACE_SECS` | Grace window for the previous key after a rotation (default `90`, bounded `5..=600`). |

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

### Kernel WireGuard lifecycle (`--mesh-kernel`)

With `--mesh-kernel`, the service selects `KernelMeshBackend` instead of the
userspace boringtun manager. The kernel link is the data transport; no TUN
device is created and no packet pump is started, even if `--mesh-dataplane`
flags are also present (`--mesh-tun`/`--mesh-mtu`/`--mesh-prefix-len` supply the
kernel link name and dimensions).

Start order is: validate settings and host prerequisites, create the link,
write the private key to a short-lived mode-0600 file, `wg set` the key and
listen port, set MTU/address/up, then add each peer and its `/32` route. Any
failure rolls back the applied steps in reverse. Stop removes routes, peers, and
the link in reverse and is idempotent. Peer add/remove and discovery both update
`wg set` plus the route table; a route failure rolls the peer change back.

Kernel identity and status are honest:

* `identity.wireguard` is `"kernel"`;
* `kernel` is a separate status document with `interface`, `public_key` (never
  the private key), `listen_port`, `address`, `mtu`, `up`, `routes`, and peers
  parsed from `wg show <if> dump` (handshake age, tx/rx, allowed IPs, endpoint,
  keepalive);
* a peer is `established` only when the kernel reports a non-zero handshake
  within WireGuard's 180 s reject time. Configuration alone stays `idle`.

Typed kernel prerequisites are returned as 501 documents: `mesh.root-required`
for a non-root process, and `mesh.dataplane-unavailable` for a missing
`wireguard` module, `ip`, or `wg`. `wg show dump` output contains a private-key
column; it is parsed and discarded internally and is never logged or serialized.

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

### Key rotation and grace window

`POST /api/v1/connect/rotate` (bearer-guarded like the other control routes)
requires an explicit `{"confirm":true}` body. An optional `grace_secs` selects a
window bounded to `5..=600` seconds; the default is 90. Rotation also runs
automatically only when `--mesh-rotate-hours` is non-zero; the scheduler adds
`0..=--mesh-rotate-jitter-secs` and stops with the service.

Userspace backend:

1. a fresh X25519 keypair is generated;
2. every static/discovered peer gets a new boringtun tunnel under the new local
   key; if any tunnel cannot be created, nothing is committed and
   `mesh.rotation-failed` is returned;
3. the advertisement is re-registered with the new `pubkey` and the old key as
   `prev_pubkey`; an mDNS failure also rolls back;
4. the old tunnels are retained as grace tunnels. Datagrams/handshakes that the
   new tunnel rejects are retried on the old tunnel until the window expires;
5. expiry drops the old tunnels and re-advertises without `prev_pubkey`.

Kernel backend: the interface key is replaced and every `wg set peer` entry is
re-applied. A peer failure reinstalls the previous key and peer configuration
and returns `mesh.rotation-failed`. Kernel WireGuard has one interface key, so
the previous key is retained for rollback/status rather than kept concurrently
usable for inbound packets; the userspace backend is the one with true dual-key
grace. The kernel `KernelWireguard::rotation_status` and node status never
include private material.

Rotation status is exposed as `rotation` in `/api/v1/connect/status`:

```json
"rotation": {
  "key_id": 2,
  "public_key": "8yIK...sDA=",
  "fingerprint": "wg:5114a2e59eee9610",
  "last_rotation_unix_ms": 1795621408539,
  "grace_remaining_secs": 56,
  "previous_fingerprint": "wg:66313a5af39cba92",
  "grace_secs": 60,
  "scheduled": {
    "interval_hours": 24,
    "jitter_secs": 300,
    "grace_secs": 90,
    "next_rotation_unix_ms": 1795707800000
  }
}
```

Only public keys/fingerprints, generation, timestamps, and schedule bounds are
reported. Private keys are absent from status, events, argv, and logs.

For discovery, a peer may change its WireGuard public key only when its mDNS
TXT record carries a `prev_pubkey` matching the currently trusted/known key.
The discovery engine emits `DiscoveryAction::KeyRotated`, which updates the peer
entry and re-initiates the handshake. A key change without that proof remains a
`mesh.peer-conflict`. A static `--mesh-peer` entry is still authoritative and is
never replaced by a rotation advertisement.

Typed prerequisites (`MeshServiceError.code`):

| Code | Meaning |
|---|---|
| `mesh.not-configured` | Missing identity/IP/port. |
| `mesh.ip-invalid` | `--mesh-ip` is not IPv4. |
| `mesh.key-missing` | No readable key file. |
| `mesh.key-invalid` | Key file is not valid base64 X25519 material. |
| `mesh.root-required` | Kernel mode requested, node is not root. |
| `mesh.wg-missing` | An explicit `requires_wg` config requested kernel mode and `wg` is not on `PATH`. |
| `mesh.wireguard-unavailable` | No usable userspace or kernel backend. |
| `mesh.peer-invalid` | Configured peer JSON/address or `--mesh-peer-endpoint` entry is invalid. |
| `mesh.discovery-invalid` | Discovery bounds or a `--mesh-trust` entry is invalid. |
| `mesh.rotation-invalid` | Rotation grace/interval/jitter is outside its bounds, or the request grace is invalid. |
| `mesh.rotation-failed` | A peer/link could not be updated; the backend rolled back to the old key. |
| `mesh.rotation-unconfirmed` | The rotate route was called without `{"confirm":true}`. |
| `mesh.rotation-unsupported` | The selected backend does not implement rotation. |
| `mesh.peer-conflict` | A discovered advertisement conflicts with a static/other peer. |
| `mesh.peer-unauthorized` | An advertisement lacks a matching key/host authorization. |
| `mesh.peer-already-exists` / `mesh.peer-not-found` | Typed peer-table errors. |
| `mesh.dataplane-unavailable` | The kernel `wireguard` module, `ip`, `wg`, `/dev/net/tun`, or (for NAT) `nft` is unavailable. |
| `mesh.dataplane-invalid` | TUN/kernel interface name, address/prefix, MTU, peer route, NAT table/interface, or a kernel/mesh identity mismatch is invalid. |
| `mesh.dataplane-not-configured` | A dataplane operation was attempted with no configuration. |
| `mesh.dataplane-command-failed` | `ip`, `wg`, or `nft` exited non-zero; the applied steps are rolled back. |
| `mesh.tun-open` | The persistent TUN fd could not be attached for the packet pump. |
| `mesh.pump-*` | Typed packet-pump read/write/tunnel/queue failure; the pump stops and reports `failed`. |
| `mesh.start-failed` | Runtime bind/mDNS/start failure. |
| `mesh.not-running` | Peer operation attempted while stopped. |

Prerequisite codes are returned as typed HTTP 501 documents; runtime failures
are reported in the status document (`state:"failed"`, `code`, `reason`) and do
not fabricate peers.

## Node routes

All four routes are merged into `controller_compat::compat_router` **before**
the node API bearer layer, so they use the same `--api-token` guard as
`/input/*`, `/power/*`, `/codec`, and `/usb`.

* `GET /api/v1/connect/status`
* `GET /api/v1/connect/peers`
* `GET /api/v1/connect/events?limit=N`
* `POST /api/v1/connect/rotate` (`{"confirm":true,"grace_secs":90}`)

Rotation responses are `200` on success (`rotation` object), `400` for an
unconfirmed/invalid request, `409` while the service is stopped, `501` for an
unsupported backend, and `500` with a typed `mesh.rotation-failed` when a peer
update was rolled back.

Status includes:

* `capability:"mesh"`, `capabilities:["/api/v1/connect/status",
  "/api/v1/connect/peers","/api/v1/connect/events",
  "/api/v1/connect/rotate"]` (discoverable even while disabled);
* `enabled`, `state`, `available`, `wireguard` (`userspace-boringtun`, `kernel`,
  `not-configured`, `fake`, or `unavailable`);
* optional `identity`, `dataplane`, `kernel`, `pump`, `discovery`, `rotation`,
  `peer_count`, `peers`, `event_count`, `event_capacity`;
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

* `cargo test -p ozma-mesh -p ozma-node --lib` — **785 passed, 7 ignored**
  (`ozma-mesh`: 74 passed, 2 ignored; `ozma-node`: 711 passed, 5 ignored).
* `cargo test -p ozma-node --bin ozma-node mesh_` — **3 passed** (CLI defaults,
  trust parsing, and rotation flag bounds).
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
* kernel backend argv/order, key redaction, typed root/module/tool errors,
  peer add/remove and route rollback, `wg show dump` status parsing, static
  underlay endpoint override, and node kernel-backend selection/preflight;
* controller token forwarding, status/capability discovery, typed 501
  passthrough, Python-node 404, auth-failure mapping, and unchanged global
  Connect status;
* key rotation: userspace rotate/grace/rollback, kernel key reinstall and peer
  re-apply with rollback, status/API confirmation and bounds, no private-key
  leakage, authorized discovery key rotation, and scheduled rotation.

No default test requires root, a real WireGuard interface, a real TUN device, a
network namespace, or a real peer. The live dataplane and live TUN-attach tests
are `#[ignore]`d and additionally gated by `OZMA_MESH_LIVE=1`. The kernel
two-namespace live runs are external harnesses recorded in
[`phase4-mesh-kernel-report.md`](phase4-mesh-kernel-report.md); they are not part
of the default suite.

## Honest remaining gaps (modelled or unwired)

* **Kernel mode requires an operator-provisioned host.** The module is never
  loaded automatically; a missing module/`ip`/`wg`/root is a typed 501. There
  is no capability probe beyond `/sys/module/wireguard`, `PATH`, and geteuid.
* **Kernel static peers need an underlay endpoint when the mesh IP is routed
  through `wg`.** The transport endpoint cannot be the overlay address in that
  topology (kernel WireGuard would recurse into its own link, and policy routes
  did not redirect its transport on the live kernel). Use
  `--mesh-peer-endpoint ID=ADDRESS:PORT`; discovered peers already use the
  observed mDNS underlay address. Without an override the peer is still
  configured and its `/32` route is programmed, but transport may not pass.
* **Kernel mode does not program forwarding or NAT.** It creates one `wg` link
  with addresses and peer `/32` routes only. `--mesh-dataplane-nat` is not
  applied in kernel mode.
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
* **Rotation does not persist the key file.** The node rotates its in-memory
  identity and advertises the new key, but the operator-provided
  `--mesh-key-file` is not rewritten. A restart resumes the original file key
  (the key id counter resets); either rewrite the file out of band or rotate
  again after restart. No generated identity is persisted by this change.
* **Kernel grace is rollback-only.** Kernel WireGuard exposes one interface
  private key. A kernel rotation switches that key immediately; the previous
  key stays in memory for rollback/status until grace expiry but cannot accept
  old-key handshakes in parallel. Userspace boringtun keeps both tunnels live
  for the window.
* **No preshared keys yet.** The grace machinery retains the previous local
  private key (and the old tunnel). A future PSK would need the same retention
  point; today every tunnel is created with `preshared_key: None`.
* **Rotation advertisements are still unauthenticated mDNS.** A key change is
  accepted only when `prev_pubkey` matches the existing trusted/known key, but
  mDNS itself has no signature. Trust entries are updated in memory as
  rotations are observed; a peer restart falls back to the configured key and
  may reject a later rotation until its trust file is updated.
* **No controller cloud relay integration.** This wires node-local mesh state
  only. Ozma Connect relay registration, relay mesh IP assignment, controller
  mesh CA pairing, and cloud failover remain Python-side/cloud concerns.
* **No live controller ↔ Rust-node mesh test.** Controller tests use a fake HTTP
  node; no real node process, WireGuard handshake, or `/api/v1/connect/*` call
  was exercised over the network in this change.
