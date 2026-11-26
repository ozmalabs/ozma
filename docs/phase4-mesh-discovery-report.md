# Phase 4 — Mesh peer discovery and route reconciliation (report)

Date: 2026-11-25 (UTC)
Host: Linux, `iproute2` from the live netns run
Tree: `4547cd4` plus the uncommitted mesh discovery/reconciliation changes
described here. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were not touched. Nothing was committed.

## Verdict

**PASS.** The mesh service now has opt-in mDNS browsing, key/host-authorized
peer admission, bounded dedupe/expiry, `/32` route add/remove/change through the
existing dataplane seam, and status/events for discovered peers, route changes,
and conflicts. Static `--mesh-peer` entries remain authoritative.

Live two-node netns validation passed:
two nodes discovered each other over the veth, authorized each other by key,
added routes, completed a WireGuard handshake, and pinged across the overlay
`5/5` and `4/4` packets. After one node left, the remaining node expired the
peer, removed its route, and reported both events. A second live pass removed
the old policy-routing workaround entirely and still passed, because discovered
peers use the observed underlay address as their WireGuard UDP endpoint.

## Wire format and service identity

Mesh discovery uses a dedicated mDNS type:

```
_ozma-mesh._udp.local
```

This is deliberately separate from the node's controller-facing
`_ozma._udp.local` record (HTTP port + node TXT), so the two advertisements
cannot overwrite each other's instance name.

`MeshManager` registers one instance per node with these TXT entries:

| TXT key | Meaning |
|---|---|
| `id` | Authoritative node id (instance name is only a sanitized label). |
| `mesh_ip` | Overlay IPv4 address. |
| `pubkey` | Base64 X25519 WireGuard public key. |
| `wg_port` | WireGuard UDP port (also present in SRV). |
| `cap` | Comma-separated capabilities (bounded/sorted). |
| `proto` | Schema version (`1`). |
| `key_id` | Public cluster-key id, when `--mesh-cluster-key-file` is configured. |
| `ts` | Unix seconds at signing time. |
| `nonce` | Fresh random nonce (128-bit hex) bound to the signature. |
| `sig` | Base64 Ed25519 signature over the canonical payload. |

Signed TXT values are bounded well below one DNS character-string (the 32-byte
signature is 88 base64 characters), and the private cluster key never appears in
TXT, logs, status, or events. See `docs/phase4-mesh-discovery-auth-report.md`.

Browsing sends a PTR query for `_ozma-mesh._udp.local` to `224.0.0.251:5353`
with the DNS QU (unicast-response) bit from an ephemeral UDP socket. The local
`libmdns` responder keeps port 5353, so browsing does not conflict with
advertising. `MdnsBrowser` parses and assembles PTR/SRV/TXT/A records per
instance over a two-second merge window. The response source address is
preferred over an A record as the peer's underlay endpoint; A records on a
multi-homed host can list unrelated bridge addresses.

The DNS parser is bounded: 512-record cap, 128-step compression limit, explicit
truncation/bounds errors, and no panics on malformed packets.

## Trust model

An advertisement is unauthenticated by mDNS itself and is never believed on its
own. Precedence is:

1. **Static `--mesh-peer`** entries are authoritative out-of-band. An exact
   match is `StaticMatched` (a signature is not required); a different
   key/IP/port for that id remains a conflict.
2. **Cluster key required when configured.** With
   `--mesh-cluster-key-file`, every non-static advertisement must carry a valid
   Ed25519 signature from the derived cluster public key, a timestamp inside the
   bounded skew window (default `120 s`), and a non-replayed nonce. Missing,
   invalid, tampered, or expired signatures never reach the peer table.
3. **`--mesh-trust` remains a fallback and an optional restriction.** Without a
   cluster key, discovery still requires an id/key trust entry exactly as
   before. With a cluster key, a matching trust entry, when present, is enforced
   as an additional key/IP allowlist; an id with no trust entry is authorized by
   the valid cluster signature alone.

A static id with a different key/IP/port, or a second id claiming a static
peer's mesh IP, is a conflict: the static entry and route are untouched. A
discovered id changing its WireGuard public key is also a conflict unless the
old key is advertised in the signed `prev_pubkey` grace field. Two discovered
ids cannot share one mesh IP.

Typed failures:

| Error | Code at the node | Meaning |
|---|---|---|
| `MeshError::DiscoveryInvalid` | `mesh.discovery-invalid` / `mesh.peer-invalid` | Malformed id/IP/key/port, capability bounds, or bad policy. |
| `MeshError::DiscoveryUnauthorized` | `mesh.peer-unauthorized` | No trust entry, or advertised key/IP does not match it. |
| `MeshError::DiscoverySignatureInvalid` | `mesh.discovery-signature-invalid` | Missing/mismatched/invalid cluster signature. |
| `MeshError::DiscoveryStale` | `mesh.discovery-stale` | Timestamp outside the bounded skew window or a nonce replay. |
| `MeshError::DiscoveryConflict` | `mesh.peer-conflict` | Static/other peer collision or key change. |
| `MeshError::KeyError` / `Base64` | `mesh.peer-invalid` | Public key is not 32-byte base64 X25519 material. |

`MeshManager::add_discovered_peer_with_endpoint` re-validates the peer before
creating a tunnel, so a caller cannot bypass discovery validation. No peer,
route, or handshake is created for an unauthorized or malformed advertisement.

## Bounds, dedupe, and expiry

`DiscoveryConfig` carries:

* `max_peers` — default `32`, hard limit `256`;
* `ttl` — default `60 s`, accepted range `1..=3600 s`;
* `query_interval` — default `5 s`, accepted range `1..=60 s`;
* static peers and trust entries.

`DiscoveryEngine` is a deterministic, I/O-free state machine. It dedupes by id
and mesh IP, ignores this node's own advertisement, counts duplicates, and
expires peers after the TTL. A mesh-IP or underlay-endpoint change produces an
`Update` carrying the previous node; a first sight produces `Add`; expiry
produces `Remove`. All of these are unit-tested with synthetic browse streams.

Status counters (per node):

```
advertisements, authorized, added, updated, removed, duplicates, expired,
conflicts, unauthorized, invalid, limit_dropped, self_ignored, static_matched
```

## Route and tunnel reconciliation

`MeshService` observes discovery actions after the backend and dataplane are
running:

* **Add** — `MeshManager` creates one tunnel tagged `discovered`, the dataplane
  adds `ip route replace <mesh-ip>/32 dev <tun>`, then a WireGuard handshake is
  initiated. A route failure removes the peer and forgets the engine entry so
  the next advertisement retries.
* **Update** — the existing tunnel for that id is replaced (never duplicated),
  the new route is added, the old route is removed when the mesh IP changed, and
  the handshake is re-initiated. A failed new route restores the previous peer
  and retries later.
* **Remove** — the discovered tunnel is removed and the `/32` route deleted;
  this is idempotent if the route was already absent.
* **Conflict / Unauthorized / Invalid / Limit** — logged as `conflict`/`error`
  events; no tunnel, route, or handshake change.

Static `--mesh-peer` routes are still programmed at dataplane start from the
static configuration. Discovery never removes or rewrites them.

Events are recorded in the existing bounded log: `peer-discovered`,
`peer-removed`, `route`, `tunnel-state`, `conflict`, and `error`. Status adds a
`discovery` document with the policy, discovered peers (including underlay IP,
capabilities, age, remaining TTL), and the counters above.

## Endpoint separation

Static peers still use `mesh_ip:wg_port` as their WireGuard UDP endpoint, which
is why the prior static live test needed namespace policy routing. Discovered
peers carry the observed underlay address from the mDNS response and use
`underlay_ip:wg_port` for WireGuard transport while retaining `mesh_ip` as the
overlay identity. The manager matches incoming UDP by the stored endpoint, so a
handshake response from `10.10.x.x:51820` is accepted even though the peer's
mesh IP is `10.200.1.x`.

This is what allowed the live rerun below to delete the policy rule, table 100
route, and static neighbor entry entirely.

## Tests

Run for this change:

* `cargo test -p ozma-mesh -p ozma-node --lib` — **895 passed, 10 ignored**
  (`ozma-mesh`: 94 passed, 2 ignored; `ozma-node`: 801 passed, 8 ignored).
* `cargo test -p ozma-node --bin ozma-node mesh_` — **2 passed** (discovery
  flag defaults, trust parsing).
* `cargo check -p ozma-mesh -p ozma-node --all-targets` — passes (pre-existing
  warnings only).
* `rustfmt --edition 2021 --check` on every touched Rust file — passes.

Coverage highlights:

* fake browse stream: authorized add, duplicate dedupe, TTL expiry, mesh-IP
  change, max-peer bound, static conflict, own-advertisement ignore;
* typed validation: bad base64 key, non-IPv4 overlay address, zero port,
  malformed DNS packets, compression-loop/truncation rejection;
* manager: invalid key installs no tunnel; update replaces one tunnel;
  static peer cannot be overwritten or discovery-removed; duplicate mesh IP is
  rejected;
* mDNS wire assembly: PTR + SRV + TXT + A become one validated descriptor;
* node service: no handshake/route before authorization, route add/remove,
  status counters/peer origin/endpoint, `peer-discovered`/`route`/`conflict`
  events, restart without stale deduplication;
* cluster signatures: sign/verify round-trip, tampered payload/signature,
  expired/future timestamp, wrong/missing key, nonce replay, signed rotation
  with `prev_pubkey`, bounded TXT, and node-level typed
  `mesh.discovery-signature-invalid` / `mesh.discovery-stale` events.

No default test binds a multicast socket, requires root/TUN, or touches a real
network namespace. The live test below was run manually and cleaned up.

## Live validation (netns/veth)

Topology (same as the dataplane live report, with discovery instead of static
peers):

```
netns ozma-da                              netns ozma-db
veth-da 10.10.10.1/24  <──veth pair──>  veth-db 10.10.20.1/24
route 10.10.20.0/24, 224.0.0.0/4        route 10.10.10.0/24, 224.0.0.0/4
ozma0 10.200.1.1/24                     ozma0 10.200.1.2/24
WG UDP :51820                           WG UDP :51820
ozma-node --mesh --mesh-discover        ozma-node --mesh --mesh-discover
  --mesh-trust ozma-disc-b=<B key>        --mesh-trust ozma-disc-a=<A key>
```

No `--mesh-peer` entries were supplied, so the peer table was populated only by
authorized discovery. Both nodes used fresh X25519 keys. mDNS multicast route
`224.0.0.0/4 dev veth` was added inside each namespace.

### Discovery and establishment

Immediately after start, each node logged the authorized add and route:

```
A: MeshManager: discovered peer added peer_id=ozma-disc-b peer_ip=10.200.1.2
   endpoint=10.10.20.1:51820
A: WireGuard: handshake initiation sent peer_id=ozma-disc-b
   endpoint=10.10.20.1:51820
```

Status (abridged):

```json
A peer_count 1
  peer ozma-disc-b mesh_ip=10.200.1.2 origin=discovered
       capabilities=[audio,hid,video] endpoint=10.10.20.1:51820
       tunnel=established established=true age=7
  routes=["10.200.1.2/32"]
  discovery counters: advertisements=4 authorized=2 added=1 duplicates=1
       self_ignored=2 conflicts=0 unauthorized=0 invalid=0

B peer_count 1
  peer ozma-disc-a mesh_ip=10.200.1.1 origin=discovered
       capabilities=[audio,hid,video] endpoint=10.10.10.1:51820
       tunnel=established established=true age=7
  routes=["10.200.1.1/32"]
```

The `self_ignored` count is each node's own mDNS advertisement; `duplicates`
are repeated browse responses.

### Ping through the overlay

```
$ sudo ip netns exec ozma-da ping -c 5 -i 0.5 -W 2 10.200.1.2
64 bytes from 10.200.1.2: icmp_seq=1 ttl=64 time=0.533 ms
64 bytes from 10.200.1.2: icmp_seq=2 ttl=64 time=1.02 ms
64 bytes from 10.200.1.2: icmp_seq=3 ttl=64 time=0.716 ms
64 bytes from 10.200.1.2: icmp_seq=4 ttl=64 time=0.607 ms
64 bytes from 10.200.1.2: icmp_seq=5 ttl=64 time=0.670 ms
5 packets transmitted, 5 received, 0% packet loss
rtt min/avg/max/mdev = 0.533/0.709/1.019/0.166 ms
```

### No static-peer policy routing needed

For the control comparison, the discovery run was restarted after deleting the
old workaround state (`ip rule priority 100`, table 100 routes, static neighbor
entries). Discovery learned the peers again and ping still passed:

```
A peers [('ozma-disc-b', '10.10.20.1:51820', established=True)]
B peers [('ozma-disc-a', '10.10.10.1:51820', established=True)]
4 packets transmitted, 4 received, 0% packet loss
```

This validates that overlay routes (`/32 dev ozma0`) and transport endpoints
(`10.10.x.x:51820` on the veth) are separated for discovered peers.

### Peer leaves: tunnel and route removed

Node B was sent `SIGTERM`. After the configured 10-second TTL, A reported:

```json
state="running" peer_count=0 routes=[]
discovery={"discovered_count":0,"counters":{"added":1,"removed":1,
           "expired":1,"duplicates":3,"conflicts":0,"unauthorized":0}}

events:
  5 peer-discovered ozma-disc-b  discovered peer ... (key authorized)
  6 route           ozma-disc-b  route added ... (10.200.1.2/32)
  7 tunnel-state    ozma-disc-b  peer ... tunnel is established
  8 route           ozma-disc-b  route removed ... (10.200.1.2/32)
  9 peer-removed    ozma-disc-b  discovered peer ... expired after the discovery TTL
```

Kernel routes inside A:

```
before: 10.200.1.2 scope link          (plus the kernel 10.200.1.0/24 link route)
after:  only 10.200.1.0/24 proto kernel scope link src 10.200.1.1
```

### Cleanup

Both nodes were stopped, all test namespaces and the veth pair were deleted, the
temporary key/log/evidence directory was removed, and the host was verified to
have no `ozma-da`/`ozma-db` namespace, no test veth, and no `ozma-node`
process. No host default route, DNS, firewall, or real interface was changed.

## Remaining gaps

1. **Authentication is a shared cluster key, not per-node identity.** With
   `--mesh-cluster-key-file` every advertisement is Ed25519-signed, verified,
   freshness-checked, and replay-protected; without it the older static/
   `--mesh-trust` model still applies. There is no per-node certificate, key
   revocation, or controller-issued mesh CA yet, and the shared private key must
   be distributed out of band (mode `0600`).
2. **No IPv6 discovery or transport.** The advertisement and dataplane are
   IPv4-only.
3. **Underlay endpoint is observed, not configured.** The mDNS response source
   is used as the endpoint. A future explicit `endpoint` TXT field would let a
   peer advertise a preferred transport address (for example a routed/WAN
   address) distinct from the link-local observation. There is no endpoint
   stability hysteresis: a multi-homed peer whose response source changes can
   briefly replace its tunnel/route.
4. **Static peers still use `mesh_ip:wg_port` as transport.** Existing
   `--mesh-peer` semantics are unchanged, so a static-only deployment still
   needs an endpoint-separation strategy; discovered peers do not.
5. **No controller/cloud integration.** Discovery is node-local; relay
   registration and controller-driven trust distribution remain out of scope.
