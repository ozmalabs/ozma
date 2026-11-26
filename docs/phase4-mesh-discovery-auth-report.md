# Phase 4 — Authenticated mesh discovery (report)

Date: 2026-11-26 (UTC)
Host: Linux, userspace WireGuard, `iproute2`; two live network namespaces
Tree: committed base plus the uncommitted discovery-authentication changes
described here. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were intentionally not edited. Nothing was
committed.

## Verdict

**PASS.** `_ozma-mesh._udp` advertisements can now be signed and verified with a
shared Ed25519 cluster key (`--mesh-cluster-key-file`, mode `0600`). A peer
refuses advertisements with a missing, invalid, or tampered signature, a stale
timestamp, or a replayed nonce before any tunnel, route, or handshake is
installed. Static `--mesh-peer` entries remain authoritative, and the previous
`--mesh-trust` model remains the fallback when no cluster key is configured.

Live two-namespace validation passed with only a shared cluster key and no
`--mesh-trust` entries: both nodes discovered each other, programming the
observed underlay endpoint, and completed a WireGuard handshake. Restarting one
node with a different cluster key caused the other node to reject every
advertisement as `mesh.discovery-signature-invalid`; the old peer expired after
the discovery TTL. No private key appeared in TXT, status, events, or logs.

## Configuration

| Flag | Meaning |
|---|---|
| `--mesh-cluster-key-file PATH` | Shared Ed25519 signing key. When set, advertisements are signed and discovered advertisements must verify. Requires `--mesh`. |
| `--mesh-trust ID=KEY` / JSON | Optional per-node allowlist. Fallback when no cluster key is set; additional key/IP restriction when a cluster key is set. |
| `--mesh-peer JSON` | Static peer, still authoritative and pre-trusted out of band. |

The file may contain a 32-byte seed, a 64-byte libsodium private key
(`seed || public`), or their base64/hex text forms. On Unix the file must not be
group/other-readable (`0600` or `0400`); a broader mode is a typed
`mesh.cluster-key-invalid` startup failure. `--mesh-cluster-key-file` without
`--mesh` is rejected by clap.

## Signature format

The advertiser signs a canonical, line-oriented payload:

```text
ozma-mesh-ad-v1
id:<node id>
mesh_ip:<overlay IPv4>
pubkey:<base64 X25519 WireGuard public key>
wg_port:<u16>
prev_pubkey:<sorted comma-separated rotation keys>
ts:<unix seconds>
nonce:<128-bit hex>
cap:<sorted comma-separated capabilities>
```

Values are the same trimmed/normalized fields the browser reconstructs from TXT.
Previous keys and capabilities are sorted and deduplicated so record order
cannot change the signature. The payload inherits the existing id, capability,
and previous-key limits and is therefore bounded (a normal advertisement is well
under 512 bytes; the theoretical maximum with 16 maximum-length capabilities is
about 1.2 KB). Every TXT property remains short enough for one DNS
character-string.

The mDNS TXT record adds four bounded fields:

| TXT key | Meaning |
|---|---|
| `key_id` | `sha256(public_key)[..8]` as 16 hex chars. Public only. |
| `ts` | Unix seconds at signing time. |
| `nonce` | Fresh random 16-byte hex nonce per signature/registration. |
| `sig` | Base64 Ed25519 signature over the canonical payload (88 chars). |

The Ed25519 private seed is held only in `ClusterSigningKey`; `Debug` prints the
key id and public key only. The type is not serializable, and it is never placed
in TXT, status, events, logs, or command lines. Status reports only
`discovery.auth = {enabled, key_id, max_skew_secs}`.

## Verification and precedence

`DiscoveryEngine` performs signature verification after structural validation and
after the static-peer authority checks. Precedence is:

1. **Static `--mesh-peer`:** an exact match is `StaticMatched` and is not
   duplicated as a dynamic peer. A different key/IP/port for the same id remains
   a conflict. Static configuration is trusted out of band and does not need a
   signature.
2. **Cluster key configured:** every non-static advertisement is rejected unless
   `key_id` matches the configured key, the Ed25519 signature verifies over the
   canonical payload, `ts` is inside the default `±120 s` window, and the nonce
   has not already been bound to a different signed payload. The failure is
   `mesh.discovery-signature-invalid` or `mesh.discovery-stale`.
3. **`--mesh-trust` fallback/restriction:** without a cluster key, the old model
   still requires a matching id/key trust entry (optional mesh-IP restriction).
   With a cluster key, a matching trust entry is still enforced if present; an
   id with no trust entry is authorized by cluster membership alone.

Static peers are checked before signatures so an existing deployment with only
`--mesh-peer` is unchanged. A configured cluster key therefore cannot be
bypassed by an unsigned `--mesh-trust` or unknown advertisement.

### Freshness and replay

`ts` must be no more than the configured skew in the past or future (default
`120 s`, hard maximum `3600 s`). `MeshAdvertiser` re-signs and re-registers a
signed advertisement every `30 s` while the service is running, so the timestamp
stays fresh; both the userspace manager timer and a service-level refresh task
drive this.

Nonces are tracked in a bounded (4096-entry) replay cache. Normal mDNS
re-announcements reuse the same signed advertisement and are treated as
duplicates by the existing dedupe path. If the same nonce appears with a
different signed payload, the advertisement is refused as
`mesh.discovery-stale` ("replayed nonce"). Entries are pruned after twice the
skew window.

### Rotation

Rotation advertisements use the same signer and cover both the new `pubkey` and
the advertised `prev_pubkey` grace keys in the signed payload. A rotation cannot
be forged by removing or changing the grace list without invalidating the
signature. The existing in-memory trust-follow logic still only accepts a key
change when the old key is in `prev_pubkey`.

## Code changes

* `ozma-mesh/src/discovery_auth.rs` (new): `ClusterSigningKey`,
  `DiscoveryAuthConfig`, `AdvertisementAuth`, canonical payload, key-file
  loading/mode enforcement, key ids, freshness checks, and unit tests.
* `ozma-mesh/src/discovery.rs`: `RawAdvertisement` auth fields from TXT, engine
  `auth` policy, wall-clock `observe_at`, replay cache, typed
  `SignatureInvalid`/`Stale` actions, counters, and status auth document.
* `ozma-mesh/src/manager.rs`: `MeshAdvertiser` signing and periodic refresh;
  bounded `advertisement_properties`; `MeshManager` signer plumbing and
  signed rotation/grace re-advertisements.
* `ozma-node/src/mesh_service.rs`: `cluster_key` config, backend plumbing,
  signed kernel/userspace advertisements, service refresh task, typed
  `mesh.discovery-signature-invalid` / `mesh.discovery-stale` events.
* `ozma-node/src/main.rs`: `--mesh-cluster-key-file` /
  `OZMA_MESH_CLUSTER_KEY_FILE`, typed load failure, flag tests.
* `ozma-mesh/src/error.rs`: typed `DiscoverySignatureInvalid` /
  `DiscoveryStale` errors.
* `ozma-mesh/Cargo.toml`: `ed25519-dalek` and `sha2` (dependent crates already
  existed in `Cargo.lock`).

## Tests

Run for this change:

* `cargo test -p ozma-mesh -p ozma-node --lib` — **895 passed, 10 ignored**
  (`ozma-mesh`: 94 passed, 2 ignored; `ozma-node`: 801 passed, 8 ignored).
* `cargo test -p ozma-node --bin ozma-node mesh_` — **3 passed** (mesh flag
  defaults/parsing, including the cluster key flag).
* `cargo check -p ozma-mesh -p ozma-node --all-targets` — passes.
* `rustfmt --edition 2021` on every touched Rust file — passes.

Added deterministic coverage:

* sign/verify round-trip; wrong cluster key; missing key/signature/nonce/ts;
  tampered payload and tampered signature;
* expired and future timestamps; canonical payload order independence and bound;
* nonce replay with a different signed payload (stale) while identical repeats
  remain duplicates;
* state machine: cluster key requires a signature; a valid signature authorizes
  without `--mesh-trust`; wrong key/tampered payload/tampered grace list are
  refused; expired timestamp is `Stale`; signed rotation with `prev_pubkey`
  works;
* advertiser TXT: bounded properties, key id/ts/nonce/sig present, previous key
  covered, private seed absent, unsigned TXT shape unchanged;
* node service: valid signed discovery adds a peer, route, and handshake;
  unsigned/wrong-key/tampered advertisements never become peers; stale
  advertisements emit the typed event; status reports the public key id and
  counters;
* flag defaults and parsing for `--mesh-cluster-key-file`.

Default tests remain network/root free. The live run below is an external
harness and is not part of the default suite.

## Live validation (netns/veth)

Topology:

```text
host
├── netns ozma-auth-a  va 10.10.60.1/24 <──veth──> 10.10.60.2/24 vb  netns ozma-auth-b
│   userspace WireGuard :51960                              :51960
│   node API :7486                                          :7487
│   mesh IP 10.200.60.1                                     mesh IP 10.200.60.2
└── the same mode-0600 cluster key on both nodes; no --mesh-peer/--mesh-trust
```

Both namespaces also had `224.0.0.0/4 dev <veth>` so mDNS multicast stayed on
the test link. Only the two namespaces and the temporary veth pair were touched;
the harness removed both namespaces, the veth pair, key files, logs, and
processes afterwards.

### Shared cluster key: discovery and handshake

Both nodes reported `state:"running"`, the same public key id, and a discovered
peer:

```json
A discovery.auth = {"enabled":true,"key_id":"42a2f5717972e84c","max_skew_secs":120}
  counters: advertisements=4 authorized=1 added=1 duplicates=0
            conflicts=0 unauthorized=0 signature_invalid=0 stale=0
  peer ozma-auth-b 10.200.60.2 origin=discovered
       underlay_ip=10.10.60.2 endpoint=10.10.60.2:51960 established=true

B discovery.auth = {"enabled":true,"key_id":"42a2f5717972e84c","max_skew_secs":120}
  counters: advertisements=2 authorized=1 added=1
  peer ozma-auth-a 10.200.60.1 origin=discovered
       underlay_ip=10.10.60.1 endpoint=10.10.60.1:51960 established=true
```

The peer was added with no per-node trust entry, which is the cluster-membership
path. The advertisement also carried `prev_pubkey`, `key_id`, `ts`, `nonce`, and
`sig` (signature TXT is bounded).

### Different cluster key: rejected

Node B was stopped and restarted with a different mode-0600 cluster key (same
node id/mesh IP). Node A immediately emitted:

```text
conflict mesh.discovery-signature-invalid: advertisement key id
  "74ce666c332d83f9" does not match the configured cluster key 42a2f5717972e84c
```

A's counters became `signature_invalid=5`, and after the 20 s discovery TTL the
previous B peer was removed:

```json
{"peers":[], "counters":{"added":1,"removed":1,"expired":1,
 "signature_invalid":5,"stale":0,"unauthorized":0}}
```

The restarted B (with the different key) also rejected A's valid-key
advertisements: `authorized=0`, `added=0`, `signature_invalid=7`. This is the
expected mutual rejection when the cluster keys differ.

### Secret handling

The harness grepped all three node logs for each WireGuard private key and each
cluster key file; none were present. Status contained only the public
`key_id`, public WireGuard keys, and no signing material or seed.

## Remaining gaps

* **Shared key, not per-node identity.** Any holder of the cluster private key
  can sign for any node id (the WireGuard key still has to match an authorized
  advertisement and the overlay identity rules still apply). There is no
  per-node certificate, revocation, or controller-issued mesh CA yet.
* **Key distribution is out of band.** The cluster key file must be copied to
  every node with mode `0600`; no provisioning/rotation mechanism for the
  cluster key itself is provided.
* **Same-nonce repeats are duplicates.** Because a single mDNS registration is
  re-announced repeatedly, an identical signed payload is accepted as a
  duplicate (and deduped) rather than rejected; a nonce reused with different
  signed fields is refused. This keeps normal mDNS TTL refresh working while
  detecting nonce equivocation/replay with changed payloads.
* **Static peers are still out-of-band.** A static `--mesh-peer` does not need a
  signature and a remote static-key rotation still requires updating the peer
  JSON, as before.
* **Refresh interval versus skew.** Signed advertisements are refreshed every
  `30 s` against a default `120 s` window. A node suspended/partitioned for
  longer than the window stops being accepted until it re-registers after
  resume; this is intentional bounded freshness.
* **IPv6 remains unsupported.** Discovery and transport are IPv4-only.
