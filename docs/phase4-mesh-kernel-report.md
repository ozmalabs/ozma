# Phase 4 — Kernel WireGuard mesh backend (report)

Date: 2026-11-25 (UTC)
Host: Linux `7.3.5-2-cachyos`, iproute2 `7.3.0`, `wireguard-tools 1.0.20260223`,
kernel module `wireguard 1.0.0` (in-tree)
Test root: `/tmp/opencode/ozma-kernel-live*` (removed after the runs)

## Scope and verdict

`--mesh-kernel` now selects a real Linux kernel `wireguard` backend instead of
returning a typed unsupported state. The backend creates/configures its own
link, key, listen port, peers, address, MTU, and `/32` routes through the
existing `CommandRunner`/`Sysfs` seams, and reports kernel handshake/byte
counters from `wg show <if> dump`.

Two live two-namespace runs passed: one with mDNS-discovered peers and one with
static peers plus explicit underlay endpoints. Both carried ICMP over the mesh
IPs, showed real WireGuard handshakes and tx/rx counters, and left no interface,
route, or namespace on the host after `SIGTERM` and namespace deletion.

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not edited.
Nothing was committed.

## Design

`ozma_mesh::KernelWireguard` owns:

* `KernelWgSettings` — interface, mesh IPv4/prefix, MTU, listen port, keepalive;
* the local private key (never in `Debug`, argv, logs, or status);
* a peer table keyed by node id, with static/discovered origin, capabilities,
  overlay mesh IP, and transport endpoint;
* an injectable `CommandRunner`, `Sysfs`/`FileOpener`, and a short-lived
  private-key file path;
* an applied-step list for reverse rollback and idempotent stop.

The node selects `KernelMeshBackend` when `MeshServiceConfig.kernel` is set.
Kernel mode takes precedence over a TUN dataplane: it creates no TUN and starts
no userspace boringtun/packet pump. `--mesh-tun`, `--mesh-mtu`, and
`--mesh-prefix-len` supply the kernel link dimensions. The backend also
registers the same `_ozma-mesh._udp.local` advertisement as the userspace
manager, so mDNS discovery works in kernel mode.

### Key handling

`FileOpener::write_secret` was added to the dataplane seam. `SystemSysfs`
implements it with mode `0600`; simple test doubles keep the default
`write`. The backend writes the base64 private key only to that file, runs
`wg set <if> private-key <file> ...`, and removes the file immediately (also on
command failure). The path is the only key-related value in an argv.

`wg show <if> dump` begins with the interface private key. `parse_wg_dump`
discards the private and preshared key columns, and command failures go through
`redact_keys`, which replaces WireGuard-shaped base64 tokens with
`[redacted]`. Status and `Debug` output therefore contain public keys only.

## Exact command sequence

Start:

| Step | Invocation |
|---|---|
| Create link | `ip link add dev <if> type wireguard` |
| Key + port | `wg set <if> private-key <mode-0600-temp-file> listen-port <port>` |
| MTU | `ip link set dev <if> mtu <mtu>` |
| Address | `ip addr replace <addr>/<prefix> dev <if>` |
| Up | `ip link set dev <if> up` |
| Peer (each) | `wg set <if> peer <pubkey> allowed-ips <mesh-ip>/32 endpoint <addr:port> persistent-keepalive 25` |
| Route (each) | `ip route replace <mesh-ip>/32 dev <if>` |

Runtime peer add repeats the peer + route commands; peer remove runs
`ip route del <mesh-ip>/32 dev <if>` and `wg set <if> peer <pubkey> remove`.
A route failure rolls the `wg set peer`/`allowed-ips` change back. Discovery
uses the observed mDNS underlay IP as the endpoint; static peers use
`--mesh-peer-endpoint ID=ADDRESS:PORT` when supplied, otherwise
`<mesh-ip>:<wg-port>`.

Stop walks the applied list in reverse: route delete, peer remove, link delete
(`ip link delete dev <if>`). Missing links/routes/peers are treated as already
clean, so a second stop issues no commands. A start failure performs the same
rollback and keeps the list only when rollback itself failed, so a later stop can
retry.

## Status

The status document is separate from peer handshake state:

```json
"kernel": {
  "configured": true,
  "state": "running",
  "interface": "wg0",
  "public_key": "9I62...jjU=",
  "listen_port": 51920,
  "address": "10.200.20.1/24",
  "mtu": 1420,
  "up": true,
  "routes": ["10.200.20.2/32"],
  "peers": [{
    "public_key": "w3ll...jnY=",
    "endpoint": "10.10.30.2:51920",
    "allowed_ips": ["10.200.20.2/32"],
    "last_handshake_age_secs": 2,
    "tx_bytes": 1444,
    "rx_bytes": 1296,
    "persistent_keepalive": 25
  }]
}
```

`up` is true only while running. `state` is `stopped`, `starting`, `running`, or
`failed`. A `wg show` failure adds a typed `last_error` and falls back to idle,
zero-counter peers; it never invents a handshake. `GET
/api/v1/connect/status` also exposes `identity.wireguard:"kernel"` and the same
peer list as `peers`, where `established` is true only when the kernel's latest
handshake is non-zero and within 180 seconds.

## Typed failures

| Code | Cause |
|---|---|
| `mesh.root-required` | not root |
| `mesh.dataplane-unavailable` | missing `wireguard` module, `ip`, or `wg`; link creation rejects `type wireguard` |
| `mesh.dataplane-invalid` | bad interface/address/prefix/MTU/port or kernel-vs-mesh identity mismatch |
| `mesh.dataplane-command-failed` | `ip`/`wg` non-zero; details redacted; applied steps rolled back |
| `mesh.peer-already-exists` / `mesh.peer-not-found` / `mesh.peer-conflict` | typed peer-table errors, including static-vs-discovered authority |
| `mesh.not-running` | peer operation while the link is stopped |

A missing module is never faked or silently loaded: the operator gets the
`modprobe wireguard` remediation. `wireguard-tools` was installed on this host
only because it was absent (`pacman -S wireguard-tools`).

## Tests

Added deterministic tests (`ozma-mesh/src/kernel.rs`) using fake command and
filesystem seams:

1. exact start argv order for two peers, including key-file write/remove and the
   absence of the private key from every argv, `Debug`, and settings;
2. runtime peer add and remove argv (`wg set` + route replace/del);
3. peer-add rollback when the route command fails;
4. discovery add/update/remove with an underlay endpoint and one peer update;
5. static underlay endpoint override (mesh IP remains the `/32` route);
6. `wg show dump` parsing: public key, listen port, handshake age, rx/tx,
   `(none)` endpoint/allowed IPs, `off` keepalive, zero handshake;
7. private-key non-disclosure in status JSON/`Debug` and redaction of `wg show`
   failures;
8. typed preflight failures for root, module, `ip`, and `wg`;
9. typed `mesh.dataplane-unavailable` when link creation reports an unknown
   device type;
10. full start rollback (routes, peer, link) and secret removal;
11. reverse-order idempotent stop;
12. invalid configured static peer address fails before any command is issued;
13. node preflight/backend-selection tests (`--mesh-kernel` config selects the
    kernel backend, reports `kernel` identity/status, and produces no TUN/pump).

`cargo test -p ozma-mesh -p ozma-node --lib` results are recorded in the node
report. `cargo check -p ozma-mesh -p ozma-node --all-targets` and
`rustfmt --edition 2021 --check` on every touched file pass.

## Live evidence

### Topology

```
host
├── netns ozma-k-a   veth 10.10.30.1/24  <──veth──>  veth 10.10.30.2/24  netns ozma-k-b
│   wg0 10.200.20.1/24                                  wg0 10.200.20.2/24
│   WG UDP :51920                                        WG UDP :51920
│   node API :7482                                       node API :7483
└── target/debug/ozma-node --mesh-kernel --mesh-tun wg0 --mesh-prefix-len 24
```

Only the two namespaces and the veth pair were touched. No host default route,
firewall, DNS, or real interface was changed.

### Run 1 — mDNS discovery

Both nodes ran with `--mesh-discover` and a `--mesh-trust` key for the other
node, no static peer. After a namespace-local `224.0.0.0/4` route, discovery
produced one authorized peer on each side with `underlay_ip` 10.10.30.2/1. The
backend programmed `wg set ... endpoint 10.10.30.2:51920`, the `/32` route, and
the link reported:

```
A: peer w3ll...jnY= endpoint 10.10.30.2:51920 allowed 10.200.20.2/32
   latest handshake: 2 seconds ago  transfer: 1.27 KiB received, 1.41 KiB sent
B: peer 9I62...jjU= endpoint 10.10.30.1:51920 allowed 10.200.20.1/32
   latest handshake: 2 seconds ago  transfer: 1.32 KiB received, 1.36 KiB sent
```

Ping A→B: 5/5 received, 0% loss (0.061–0.804 ms). Ping B→A: 3/3 received.
`/api/v1/connect/status` on both nodes reported `state:"running"`,
`wireguard:"kernel"`, `kernel.up:true`, `routes:["<peer>/32"]`,
`origin:"discovered"`, `tunnel:"established"`, handshake age 2 s, and matching
tx/rx counters. Discovery counters showed `added:1`, `self_ignored:2`, no
unauthorized/conflict/failure.

### Run 2 — static peers with underlay endpoints

Both nodes ran with `--mesh-peer <other MeshNode JSON>` and
`--mesh-peer-endpoint ozma-ks-b=10.10.30.2:51920` (mirrored on B). The status
reported `origin:"static"`, endpoint 10.10.30.x, `routes:["10.200.20.x/32"]`,
`tunnel:"established"`, and real counters. Ping A→B was 5/5 received (0.083–
0.227 ms); B→A was 3/3. `wg show` showed real handshakes and 1.1–1.3 KiB in
each direction.

### Teardown and cleanup

Both nodes received `SIGTERM`. Each logged
`kernel WireGuard link stopped interface=wg0`; `ip link show wg0` and
`ip route show | grep wg0` were empty in both namespaces before deletion. After
`ip netns del`, the host had no `ozma-k-*` namespace, no `veth-k*`, and no
`wg0` link. Private-key files were mode `0600`, were removed from the live
status/log checks, and the private keys did not appear in either node log or
status document.

### Exact blocker found while preparing the live run

Kernel WireGuard transport to an endpoint equal to the peer's overlay mesh IP
recursed into the newly created `wg` link; namespace policy rules based on
`ipproto udp sport` or `fwmark` did not redirect it on this kernel. The raw
kernel control with an underlay endpoint worked immediately. This is why
`--mesh-peer-endpoint` exists and why discovered peers use the observed mDNS
underlay address. It is recorded as a remaining gap rather than hidden behind a
fake "established" state.

## Remaining gaps

* Static kernel peers still need an explicit underlay endpoint when their mesh
  IP is routed through the WireGuard link; there is no automatic underlay
  discovery for static configuration.
* The module is not auto-loaded; missing module/tool/root remains a typed 501.
* No forwarding or NAT programming in kernel mode, and no PMTU/ICMP handling.
* Single peer `/32` routes; no subnet/allowed-ips routing beyond the peer mesh
  address.
* No controller cloud relay/CA integration; kernel mode is node-local.
* mDNS discovery in minimal namespaces needs a multicast route
  (`224.0.0.0/4`); the node does not install one.
