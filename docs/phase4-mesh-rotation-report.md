# Phase 4 — Mesh key rotation and grace window (report)

Date: 2026-11-26 (UTC)
Host: Linux, `wireguard-tools`/`iproute2` present; two live network namespaces
Tree: committed base plus the uncommitted rotation changes described here.
`docs/parity-matrix.md` and `docs/plan-execution-status.md` were intentionally
not edited. Nothing was committed.

## Verdict

**PASS.** The mesh service can now rotate its WireGuard keypair on both
backends, re-advertise the new public key over mDNS, keep the previous key
usable for a bounded grace window, update every static and discovered peer, and
roll back to the old key with a typed error when a peer update fails. Status and
the new `POST /api/v1/connect/rotate` route expose key id/fingerprint, last
rotation, and grace remaining, with no private-key material. Optional scheduled
rotation is off by default and bounded to `1..=720` hours with jitter.

Live two-namespace validation passed: two nodes discovered each other, carried
ICMP over the userspace WireGuard dataplane, node A rotated through the API,
node B observed the `prev_pubkey` and updated its peer entry, ping stayed at
`3/3` during grace, and stayed `3/3` after the old key was removed. No private
key appeared in either status document, event log, process log, or command.

## Design

### Shared key lineage (`ozma-mesh/src/rotation.rs`)

`KeyRotation` owns the current private key, the current public key, an optional
previous key, a monotonic `key_id`, the last-rotation wall clock, and the
configured schedule. It exposes only `RotationStatus` public data:

* `key_id` — generation, starting at `1`;
* `public_key` and `fingerprint` (`wg:<16 hex>`, FNV-1a of the decoded public
  key);
* `last_rotation_unix_ms`;
* `grace_remaining_secs` / `previous_fingerprint` while a grace key is retained;
* `grace_secs` and optional schedule fields.

Grace is bounded by `validate_grace`: `5..=600` seconds, default `90`. Requests
outside the range are rejected with `mesh.rotation-invalid`, never silently
clamped. Scheduled intervals must be `1..=720` hours; jitter is capped at
`3600` seconds.

`Debug` for `KeyRotation` prints only the public key, generation, grace, and
previous public key. `WgPrivateKey` is never serialised.

### Userspace boringtun backend

`MeshManager` now keeps its identity behind a lock, a `KeyRotation` lineage, a
`TunnelFactory` seam, and a rotation mutex. `MeshManager::rotate_keys(grace)`:

1. validates the grace window and serialises concurrent rotations;
2. expires an older grace window first;
3. builds a replacement `Tunn` for **every** peer under the new local key before
   touching live state. Any failure returns `MeshError::RotationFailed` and
   leaves the old key/tunnels untouched (the rollback boundary);
4. drops the old mDNS responder and registers the new identity with
   `prev_pubkey=<old key>`; a registration failure restores the old
   advertisement and returns a typed failure;
5. commits atomically: each old tunnel becomes `grace_tunnel` with
   `grace_expires`, new tunnels become primary, the public identity changes,
   and the lineage advances;
6. after construction, the receive loop tries the current tunnel first and, on
   error, retries the grace tunnel while the deadline is live. A peer that has
   not yet learned the new key can therefore finish handshakes and keep sending
   during the window;
7. on expiry (timer tick or an explicit status/API call path) the grace tunnels
   are dropped and the advertisement is re-registered without `prev_pubkey`.

Static and discovered peers go through the same path. Adding a peer during a
grace window uses the current key only.

### Kernel backend

`KernelWireguard::rotate_keys(grace)`:

1. requires a running link and a valid grace window;
2. generates a new key and installs it through the existing short-lived
   mode-0600 file + `wg set <if> private-key <file> listen-port <port>`;
3. re-applies `wg set <if> peer <pubkey> allowed-ips … endpoint … keepalive` for
   every static/discovered peer;
4. if any peer command fails, reinstalls the previous key, re-applies every peer
   as a best effort, records `mesh.rotation-failed`, and leaves the interface
   running on the old key;
5. on success advances the lineage and clears the link's last error.

Kernel WireGuard has exactly one interface private key, so the previous key is
retained for rollback/status until grace expiry rather than being concurrently
usable for inbound handshakes. This limitation is stated in status/docs; the
userspace backend is the one with true dual-key grace.

### Discovery: authorized peer key rotation

`MeshAdvertiser::new_with_previous` publishes `prev_pubkey` (bounded to two
keys) alongside the new `pubkey`. `RawAdvertisement` and `DiscoveredPeer` carry
the parsed previous keys; `validate_advertisement` bounds them and requires
32-byte base64 X25519 values.

`DiscoveryEngine::observe` still requires a matching `--mesh-trust` entry. A key
change is accepted only when the advertised key is the trusted key, or the
advertisement lists the trusted key in `prev_pubkey`. On acceptance the
in-memory trust key follows the rotation so a second rotation chains. A known
discovered peer whose key changes emits `DiscoveryAction::KeyRotated` only when
its current key is listed as previous; otherwise it stays
`mesh.peer-conflict`. Static `--mesh-peer` entries remain authoritative.

Both backends expose `add_discovered_peer_rotated`; the node reconciler updates
the peer tunnel/link, keeps or moves the `/32` route, logs a `rotation` event,
and initiates a fresh handshake.

### Node API and scheduler

`MeshService::rotate(grace)` refuses unless the service is `running`, calls the
active backend, and records a bounded `rotation` event containing only key id,
fingerprint, and grace seconds. `MeshStatus.rotation` reports the current
lineage and, when configured, the schedule plus `next_rotation_unix_ms`.

`POST /api/v1/connect/rotate`:

```json
{"confirm": true, "grace_secs": 90}
```

* `confirm` must be exactly `true` (`400 mesh.rotation-unconfirmed` otherwise);
* grace defaults to 90 s and is bounded `5..=600`
  (`400 mesh.rotation-invalid`);
* stopped service → `409 mesh.not-running`;
* unsupported backend → `501 mesh.rotation-unsupported`;
* peer/update failure → `500 mesh.rotation-failed`, after rollback;
* success → `200` with `{ok, capability, rotation, identity, capabilities}`.

`--mesh-rotate-hours` (default `0`) enables a Tokio scheduler after the service
starts. Each delay is the configured interval plus `0..=jitter_secs`; the task
is aborted on `stop()`. A scheduled failure is logged as an `error` event and
the next interval retries without changing the key.

## Tests

`cargo test -p ozma-mesh -p ozma-node --lib` — **785 passed, 7 ignored**
(`ozma-mesh`: 74 passed / 2 ignored; `ozma-node`: 711 passed / 5 ignored).
`cargo check -p ozma-mesh -p ozma-node --all-targets` and
`rustfmt --edition 2021` on every touched Rust file pass.

Added deterministic coverage:

* `rotation.rs`: fingerprint stability/public-only output, previous-key
  retention and expiry, grace bounds, schedule bounds/jitter.
* `manager.rs`: rotation rebuilds every peer and keeps old tunnels; a
  peer-factory failure rolls the key and all peers back; grace expiry drops old
  tunnels and the previous fingerprint; a discovered key change is accepted only
  with the authorized previous key; status JSON contains no private material.
* `kernel.rs`: rotation re-applies every peer and installs exactly one new
  interface key; a `wg set peer` failure reinstalls the old key and peers; grace
  expiry drops the previous key; a discovered key change is a conflict unless
  the old key is supplied; argv/Debug/status never contain the private key.
* `discovery.rs`: a trusted key rotation without `prev_pubkey` is unauthorized;
  with it the engine emits `KeyRotated`, updates the peer, and follows the
  trust key in memory.
* `mesh_service.rs`: service rotation updates identity, rotation status, and the
  bounded `rotation` event with no private-key text; injected fake failure keeps
  the old identity/key id; the HTTP route rejects an unconfirmed request,
  rejects invalid grace, returns 409 while stopped, and returns the status
  document on success; scheduled rotation fires with a test interval and
  reports its next time; a discovery `KeyRotated` advertisement updates the
  fake peer and re-initiates the handshake.
* `main.rs`: rotation flags default off, parse explicit values, and reject
  out-of-bounds schedule/grace values.

All default tests are network/root free. The live run below is an external
harness and is not part of the default suite.

## Live evidence (two namespaces)

Topology:

```
host
├── netns ozma-rot-a  veth-ra 10.10.40.1/24 <──veth──> 10.10.40.2/24 veth-rb  netns ozma-rot-b
│   userspace WireGuard :51940                              :51940
│   node API :7482                                          :7483
│   mesh IP 10.200.40.1                                     mesh IP 10.200.40.2
└── target/debug/ozma-node --mesh --mesh-discover --mesh-dataplane
```

Only the two namespaces, the veth pair, and namespace-local multicast routes
(`224.0.0.0/4 dev veth-*`) were touched. No host interface, default route,
firewall, DNS, or TUN persisted after the run. The `.key` files were mode
`0600`, and the namespaces/processes were removed by the harness trap.

Identities before rotation:

```
A pub: JWiCMC17kETNQu4h2JBZkndCRKRtRndT7rQWGxopLBc=
B pub: fIFtHtWqiz+8nT8jzLIRpk585Yyv94pyJA/pAtFdzGU=
```

Both nodes reached `peer_count:1`, `established:true` over discovery with
`--mesh-trust <peer>=<key>`; ping A→B was `3/3` (0% loss, 0.425–1.039 ms).

`POST /api/v1/connect/rotate` with `{"confirm":true,"grace_secs":60}` returned:

```json
{
  "ok": true,
  "capability": "mesh",
  "rotation": {
    "key_id": 2,
    "public_key": "8yIKKla79kIeTC2bRY2XzzDqty7odJsNEbbNaiC+sDA=",
    "fingerprint": "wg:5114a2e59eee9610",
    "last_rotation_unix_ms": 1795621408539,
    "grace_remaining_secs": 60,
    "grace_secs": 60,
    "previous_fingerprint": "wg:66313a5af39cba92"
  }
}
```

Node B updated its discovered peer entry within the next discovery pass:

```
MeshManager: discovered peer public key rotated peer_id=ozma-rot-a
  old_fingerprint=wg:66313a5af39cba92 new_fingerprint=wg:5114a2e59eee9610
```

B's `/api/v1/connect/status` then reported `peers[0].wg_pubkey` equal to A's new
key and `established:true`. Ping A→B during the grace window was `3/3` (0%
loss), and A's status reported `grace_remaining_secs:56` with the previous
fingerprint. After the 60 s window elapsed, A logged
`rotation grace window elapsed; previous key removed`, its status no longer had
grace/previous fields, and ping A→B was again `3/3` (0% loss).

The harness compared both private-key files against both logs and every captured
status/rotate JSON; no private key was present.

## Remaining gaps

* **The key file is not rewritten.** Rotation changes the in-memory identity and
  advertisement, but the operator-supplied `--mesh-key-file` keeps the original
  key. A restart falls back to that file and resets the generation counter.
* **Kernel grace is rollback-only.** The kernel interface can hold one private
  key; old-key inbound handshakes end at the switch. The previous key remains
  for rollback/status until grace expiry.
* **No preshared keys.** Tunnels are created with `preshared_key: None`. The
  grace structure would need the same retention point when a PSK is added.
* **Rotation advertisements are unauthenticated mDNS.** Acceptance requires the
  advertised `prev_pubkey` to match the current trusted/known key, but mDNS has
  no signature. An attacker able to spoof the record could claim a rotation; the
  existing trust model already treats mDNS as untrusted and requires
  `--mesh-trust`, but there is no controller-issued mesh CA yet. In-memory trust
  follows rotations; a peer restart reverts to its configured key and may reject
  a later rotation until the trust file is updated.
* **Static peers do not follow remote rotation.** A `--mesh-peer` entry is
  authoritative and is not replaced by an advertisement. Rotating a static
  peer's key still requires updating the peer JSON on the other node.
* **Scheduled rotation was not live-tested.** It is covered by a short-interval
  fake-backend test; the live run used explicit API rotation.
* **No multi-hop/relay or controller CA integration.** Rotation remains
  node-local mesh behavior.
