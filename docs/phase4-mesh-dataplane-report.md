# Phase 4 — Mesh dataplane (report)

## Scope

This change adds an **opt-in, typed, injectable** kernel dataplane to
`ozma-mesh` and wires it to the node mesh service behind `--mesh-dataplane`
(default off).

It creates and configures a TUN link, assigns the mesh address/prefix, sets the
MTU, brings the link up, programs `/32` peer routes, enables IPv4 forwarding,
and optionally installs an nftables masquerade rule. It also owns the matching
bounded packet pump: the pump attaches to the persistent TUN fd created by the
dataplane and moves plaintext IPv4 packets to/from the existing userspace
WireGuard tunnels.

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were intentionally
not edited.

## Design: two seams, no shell

Everything kernel-facing is behind traits so default tests never touch the
host:

| Seam | Responsibility |
|---|---|
| `CommandRunner` | Runs exact `CommandSpec { program, args }` argv. No shell, no interpolation. |
| `FileOpener` | `exists` / `read_to_string` / `write` for `/dev/net/tun` and `/proc/sys`. |
| `Sysfs` | Extends `FileOpener` with `is_root()` and `command_available()`. |

Production values are `SystemCommandRunner` and `SystemSysfs`; tests inject
recording/failing fakes. `MeshDataplane` owns a config plus the two seams and a
mutex-protected applied-step list.

`DataplaneConfig` contains:

* `interface` (Linux name validation: 1–15 chars, letters/digits/`.`/`_`/`-`);
* `address` + `prefix_len` (unicast IPv4, prefix `1..=32`);
* `mtu` (`68..=65535`);
* `peer_routes` (valid, non-duplicate unicast peer addresses, not the local
  address);
* `forwarding` (on by default when the dataplane is enabled);
* `nat: Option<NatConfig>` (table name + optional egress interface).

## Exact command sequence

Start order:

| Step | Invocation |
|---|---|
| Create TUN | `ip tuntap add dev <if> mode tun persist` |
| MTU | `ip link set dev <if> mtu <mtu>` |
| Address | `ip addr replace <addr>/<prefix> dev <if>` |
| Link up | `ip link set dev <if> up` |
| Forwarding | write `1` to `/proc/sys/net/ipv4/ip_forward` (previous value saved) |
| Peer route (each) | `ip route replace <peer>/32 dev <if>` |
| NAT table | `nft add table ip <table>` |
| NAT chain | `nft add chain ip <table> postrouting { type nat hook postrouting priority 100 ; }` |
| NAT rule | `nft add rule ip <table> postrouting ip saddr <network>/<prefix> oifname != "<if>" masquerade` |

With `--mesh-nat-out eth0` the rule uses `oifname "eth0"` instead of the
non-mesh-egress form.

Runtime peer changes use the same route commands:
`MeshService::add_peer` adds `ip route replace <peer>/32 dev <if>` after the
backend accepts the peer; `remove_peer` deletes it. A route-add failure rolls
the peer back out of the backend. A route-delete failure is recorded as an
error event rather than fabricating a clean state.

## Rollback and idempotence

Applied steps are recorded as they succeed: link, forwarding (with the previous
sysfs value), each route, and the NAT table. On any start failure the list is
walked **in reverse**:

1. delete the NAT table (if created);
2. delete applied `/32` routes in reverse;
3. restore `/proc/sys/net/ipv4/ip_forward` (only if this process changed it);
4. `ip link delete dev <if>`.

Missing links/routes/tables during teardown are treated as already clean, so a
second `stop()` issues no commands. If teardown itself fails, the state becomes
`failed` with the typed error and the applied list is retained so a later
`stop()` can retry.

`start()` is idempotent while running and retries any leftover teardown from a
previous failed run before programming a new link.

## Packet pump

`ozma_mesh::pump::PacketPump` owns the `/dev/net/tun` fd and the two plaintext
directions:

```text
TUN reader ──bounded queue──▶ tunnel sender ──▶ boringtun ──▶ UDP
TUN writer ◀─bounded queue── tunnel receiver ◀── boringtun ◀── UDP
```

Two injectable seams keep default tests off the host:

| Seam | Production value | Test value |
|---|---|---|
| `TunIo` (`read_packet` / `write_packet`) | `SystemTunProvider` attaching to the existing persistent interface via the safe `tun-rs` wrapper | in-memory channels plus scripted short reads/partial writes |
| `TunnelIo` (`send_packet` / `recv_packet`) | `ManagerPacketIo` over the packet API on `MeshManager` | in-memory channels plus injected errors |

The outbound half is the minimal documented packet API added to `ozma-mesh`:
`MeshManager::send_ipv4` parses the IPv4 destination and looks up the peer that
owns it; it never invents a route or peer. The inbound half publishes
decapsulated IPv4 packets on a bounded `tokio::mpsc` channel taken once by
`MeshManager::take_packet_receiver`. IPv6 payloads are logged and ignored (the
pump is IPv4-only). IPv4 packets decapsulated before a subscriber takes the
channel are counted by the manager as dropped rather than claimed as delivered.

Bounds and counters:

* each direction has a bounded queue (`PumpConfig::queue_capacity`, default
  256, maximum 4096);
* each read buffer is fixed (`PumpConfig::packet_buffer`, default 65535,
  bounded `68..=65535`);
* a full queue increments the matching pump drop counter; the queue never
  grows and a slow direction cannot block the other. The manager additionally
  counts IPv4 packets dropped before a pump subscribes;
* `PumpStats` reports TUN read packets/bytes, tunnel send packets/bytes,
  tunnel receive packets/bytes, TUN write packets/bytes, both queue drop
  counters, and per-direction error counters.

A worker error stops both directions, records a typed `mesh.pump-*` error, and
leaves the pump in `failed`. The node service monitors that failure and tears
down the WireGuard manager and dataplane instead of leaving a half-live mesh.
`stop()` aborts in-flight worker futures and is idempotent; it preserves the
last real counters for diagnostics (they never increase after stop);
`start()` is idempotent while running and refuses to restart a failed pump
until it has been stopped.

Packet parsing treats a short TUN read as one complete short packet (kernel
semantics). TUN writes are retried until the whole packet is written, so a
short write cannot silently truncate an IPv4 packet.

## Status and typed errors

`DataplaneStatus` is nested under `/api/v1/connect/status` only when the
dataplane is configured:

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
  "nat": false,
  "last_error": null
},
"pump": {
  "configured": true,
  "state": "running",
  "interface": "ozma0",
  "queue_capacity": 256,
  "stats": {
    "tun_read_packets": 0,
    "tun_read_bytes": 0,
    "tun_to_tunnel_packets": 0,
    "tun_to_tunnel_bytes": 0,
    "tunnel_received_packets": 0,
    "tunnel_received_bytes": 0,
    "tunnel_to_tun_packets": 0,
    "tunnel_to_tun_bytes": 0,
    "dropped_tun_to_tunnel": 0,
    "dropped_tunnel_to_tun": 0,
    "tun_read_errors": 0,
    "tun_write_errors": 0,
    "tunnel_send_errors": 0,
    "tunnel_receive_errors": 0
  }
}
```

`up`, `routes`, `forwarding`, and `nat` are true/present only when
`state:"running"`. States are `not-configured`, `stopped`, `starting`,
`running`, and `failed`. A partially applied or rolled-back dataplane never
reports `running`.

The `pump` object is present only when a dataplane and TUN provider are
configured. Its counters are real zeroes before traffic, never fabricated
packets. Effectively `state:"running"` requires the dataplane, the WireGuard
manager, and a configured pump to all be running; a stopped/failed pump makes
the service `starting`/`failed` instead.

Typed pump errors:

| Code | Meaning |
|---|---|
| `mesh.tun-open` | The persistent TUN interface could not be attached/opened (including a missing root/permission failure). |
| `mesh.pump-tun-read` / `mesh.pump-tun-write` | A TUN read/write failed or the link disappeared. |
| `mesh.pump-tunnel-send` / `mesh.pump-tunnel-receive` | Encapsulation/decapsulation or queue delivery failed. |
| `mesh.pump-unavailable` | The pump was requested but the running tunnel exposes no packet API, or the dataplane/tunnel is not running. |
| `mesh.pump-invalid` | Queue/buffer bounds are outside the supported range. |
| `mesh.pump-packet-too-large` | A tunnel packet exceeded the fixed bounded buffer. |

Typed `DataplaneError.code` values:

| Code | Meaning |
|---|---|
| `mesh.root-required` | Not root; TUN/routes/forwarding/firewall cannot be programmed. |
| `mesh.dataplane-unavailable` | `/dev/net/tun`, `ip`, or (when NAT is on) `nft` is missing, or proc/sys is unwritable. |
| `mesh.dataplane-invalid` | Interface, address, prefix, MTU, peer route, NAT table, or egress interface is invalid. |
| `mesh.dataplane-not-configured` | A dataplane operation had no configuration. |
| `mesh.dataplane-not-running` | Runtime route update attempted while not running. |
| `mesh.dataplane-command-failed` | `ip`/`nft` exited non-zero; rollback was attempted. |

Preflight is also performed by `MeshServiceConfig::preflight_with`, so a node
started without root/TUN/`ip` (or with NAT but without `nft`) becomes a typed
unsupported 501 document before any kernel command runs.

## Node wiring

New flags / environment:

| Flag | Env | Meaning |
|---|---|---|
| `--mesh-dataplane` | `OZMA_MESH_DATAPLANE` | Enable the kernel dataplane (default off). |
| `--mesh-tun` | `OZMA_MESH_TUN` | TUN name (default `ozma0`). |
| `--mesh-mtu` | `OZMA_MESH_MTU` | TUN MTU (default `1420`). |
| `--mesh-prefix-len` | `OZMA_MESH_PREFIX_LEN` | Mesh prefix (default `24`). |
| `--mesh-dataplane-nat` | `OZMA_MESH_DATAPLANE_NAT` | Explicit NAT/masquerade opt-in. |
| `--mesh-nat-out` | `OZMA_MESH_NAT_OUT` | NAT egress interface. |

Lifecycle:

* `MeshService::start` starts the dataplane first, then the WireGuard manager,
  and only then opens the TUN fd and starts the packet pump. A manager or pump
  failure tears the already-started parts down; a dataplane failure never
  leaves a WireGuard socket bound and a pump failure never leaves a half-live
  mesh reporting `running`.
* `MeshService::stop` stops the pump first, then the dataplane and manager.
* Effective service state is `running` only when the dataplane, manager, and
  configured pump are all running. A pump failure is surfaced as `failed` with
  the typed pump code/reason and the monitor tears down the runtime parts.
* Peer `established` remains driven exclusively by boringtun handshake state.
  A running dataplane, programmed route, or running pump never promotes a
  peer.

The Python soft node is unchanged; this is node-only.

## Tests

Default tests never require root, a real TUN device, or `nft`. They use
recording fake runner/sysfs seams and in-memory TUN/tunnel channels.

Latest verification (private snapshot of the tree, to avoid concurrent edits):

* `cargo test -p ozma-mesh -p ozma-node --lib` — **543 passed, 5 ignored**
  (`ozma-mesh`: 22 passed, 2 ignored; `ozma-node`: 521 passed, 3 ignored).
* `cargo check -p ozma-mesh -p ozma-node --all-targets` — passes.
* `rustfmt --edition 2021 --check` on every touched Rust file — passes.

`ozma-mesh` packet-pump tests cover:

* TUN→tunnel and tunnel→TUN round trips through bounded in-memory channels;
* short TUN reads treated as one packet and partial TUN writes retried;
* queue overflow in both directions incrementing the correct drop counter;
* per-direction packet/byte/error snapshots;
* idempotent shutdown, no traffic after stop, and `wait_failed` returning
  `None` on a clean stop;
* tunnel send/receive and TUN read error propagation to a typed `failed`
  state;
* invalid queue/buffer bounds typed as `mesh.pump-invalid` with no start;
* one `#[ignore]` live attach test additionally gated by `OZMA_MESH_LIVE=1`,
  requiring root and `/dev/net/tun`.

`ozma-mesh` dataplane tests cover:

* exact argv/order for create, MTU, address, up, forwarding, and peer routes;
* NAT opt-in, exact nft argv, and optional egress interface;
* reverse rollback on route failure and on NAT-rule failure;
* forwarding restore and disabled-forwarding behavior;
* idempotent `start` and repeated `stop`, including no commands on second stop;
* dynamic route add/remove while running;
* typed missing root / `/dev/net/tun` / `ip` / `nft` errors before commands;
* address, prefix, MTU, interface, NAT table, and duplicate/self peer route
  validation;
* `starting` visible from the status accessor during command execution;
* one `#[ignore]` live round-trip, additionally gated by `OZMA_MESH_LIVE=1`,
  requiring root and `/dev/net/tun`.

`ozma-node` mesh service tests cover:

* dataplane not configured (field omitted) and stopped before start;
* `starting` while commands run, then `running` only when both parts are up;
* running status fields (interface/address/MTU/routes/NAT) and `up:false` after
  stop;
* failed status with typed `last_error`, no peer count/list, and no fake
  `running`;
* missing root/TUN/nft typed through the service;
* preflight root/TUN/`ip`/`nft`/invalid-config codes;
* peer add/remove programming routes without marking the peer established;
* pump absent without a dataplane, stopped before start, running with real
  zero counters, and stopped cleanly without fake packet counts;
* pump not opened when the dataplane or tunnel is not running;
* typed `mesh.tun-open` and `mesh.pump-unavailable` failures with rollback of
  the dataplane/manager and a `failed` pump status;
* asynchronous pump failure surfacing as `failed` and tearing down the
  manager/dataplane.

Live test invocation (manual, root):

```bash
sudo OZMA_MESH_LIVE=1 cargo test -p ozma-mesh --lib \
  dataplane::tests::live_dataplane_roundtrip -- --ignored
```

The packet pump itself is exercised by default tests with in-memory seams. The
optional live TUN attach path can be smoke-tested manually with root:

```bash
sudo OZMA_MESH_LIVE=1 cargo test -p ozma-mesh --lib \
  pump::tests::live_system_tun_attaches_to_the_dataplane_link -- --ignored
```

## Remaining kernel/dataplane gaps

* **Real-network coverage is still partial.** Packet round trips are tested
  through in-memory seams; the optional `#[ignore]` live dataplane test only
  checks the link, not a multi-node encrypted packet flow. No default test
  touches a real TUN, WireGuard handshake, or network namespace.
* **IPv4 only, single queue.** The pump reads/writes one packet buffer and
  ignores IPv6. It does not use TUN multi-queue, GSO/GRO, or vnet headers.
* **No per-peer queues or fairness.** All peers share one tunnel outbound path;
  a slow peer can consume the shared send budget. There is no per-peer egress
  queue, rate limit, or backpressure signal.
* **No PMTU/ICMP handling.** MTU is configured on the link, but the pump does
  not synthesise ICMP frag-needed or react to path-MTU changes.
* **No multi-node route exchange.** Peer routes come from configured
  `--mesh-peer` identities plus runtime `add_peer`; there is no routing
  protocol or mDNS-driven route reconciliation.
* **No IPv6, policy routing, or multiple tables.** Only IPv4 `/32` routes and
  the main table are used.
* **No forward-filter/firewall policy.** NAT is opt-in; there is no zone/input
  allow-list beyond the host firewall's existing policy.
* **No atomic kernel transaction.** Commands are applied sequentially and
  rolled back best-effort; a kernel/tool crash mid-teardown can leave state for
  the operator. `stop()` retries what it recorded.
* **No concurrency control across processes.** Two nodes/processes configuring
  the same interface name will conflict; the second `ip tuntap add` fails
  honestly.
* **Kernel WireGuard remains unimplemented.** The dataplane is independent of
  `--mesh-kernel`; `--mesh-kernel` still reports a typed unsupported state.
