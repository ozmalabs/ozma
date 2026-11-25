# Phase 4 — Mesh dataplane (report)

## Scope

This change adds an **opt-in, typed, injectable** kernel dataplane to
`ozma-mesh` and wires it to the node mesh service behind `--mesh-dataplane`
(default off).

It creates and configures a TUN link, assigns the mesh address/prefix, sets the
MTU, brings the link up, programs `/32` peer routes, enables IPv4 forwarding,
and optionally installs an nftables masquerade rule. It does **not** move
packets between the TUN device and boringtun yet; it programs the interface and
routing/firewall state around the existing userspace WireGuard tunnels.

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
}
```

`up`, `routes`, `forwarding`, and `nat` are true/present only when
`state:"running"`. States are `not-configured`, `stopped`, `starting`,
`running`, and `failed`. A partially applied or rolled-back dataplane never
reports `running`.

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

* `MeshService::start` starts the dataplane first, then the WireGuard manager.
  A manager failure tears the dataplane down; a dataplane failure never leaves
  a WireGuard socket bound.
* `MeshService::stop` stops both, dataplane first.
* Effective service state is `running` only when both parts are running. A
  dataplane failure is surfaced as `failed` with the dataplane code/reason.
* Peer `established` remains driven exclusively by boringtun handshake state.
  A running dataplane or a programmed route never promotes a peer.

The Python soft node is unchanged; this is node-only.

## Tests

Default tests never require root, a real TUN device, or `nft`. They use a
recording fake runner and fake sysfs.

Latest verification (private snapshot of the tree, to avoid concurrent edits):

* `cargo test -p ozma-mesh -p ozma-node --lib` — **514 passed, 4 ignored**
  (`ozma-mesh`: 12 passed, 1 ignored; `ozma-node`: 502 passed, 3 ignored).
* `cargo check -p ozma-mesh -p ozma-node --all-targets` — passes.

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
* peer add/remove programming routes without marking the peer established.

Live test invocation (manual, root):

```bash
sudo OZMA_MESH_LIVE=1 cargo test -p ozma-mesh --lib \
  dataplane::tests::live_dataplane_roundtrip -- --ignored
```

## Remaining kernel/dataplane gaps

* **No TUN packet pump.** The interface exists and is routed, but no process
  owns the `/dev/net/tun` fd or moves plaintext between it and boringtun. That
  is the next dataplane step; this change is control-plane programming only.
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
