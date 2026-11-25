# Phase 4 — Live two-node mesh dataplane report

Date: 2026-11-25 (UTC)
Host: Linux `7.3.5-2-cachyos`, iproute2 `7.3.0-1.1`
Tree: clean HEAD `c3478db` plus the uncommitted mesh/shutdown fixes described here
(userspace `boringtun 0.6.0`, `tun-rs 2.8.18`)
Test root: `/tmp/opencode/mesh-live2` (removed after the run)

## Verdict

**Overall: PASS for the data path, teardown, and peer-down reporting.** The
encrypted two-node data path works, the two formerly fatal pump inputs are now
dropped and counted, `SIGTERM` removes the TUN and routes on both nodes, and a
stopped peer expires and stops reporting `established` within the WireGuard
handshake timeout.

| # | Check | Result |
|---|---|---|
| 1 | Two netns joined by a veth pair, distinct underlay subnets | PASS |
| 2 | One `ozma-node` per netns with `--mesh*` + `--mesh-dataplane` | PASS (no TUN/pump workarounds) |
| 3a | WireGuard handshake reaches `established` (not just configured) | PASS |
| 3b | TUN interface, address, `/32` route, `ip_forward=1` in each netns | PASS |
| 3c | `ping` and UDP echo across mesh IPs through TUN + pump | PASS |
| 3d | `/api/v1/connect/status` counters increase, drops/errors zero | PASS (expected local drops counted separately) |
| 3e | Stopping one node tears down its TUN/routes | **PASS** (both nodes) |
| 3f | Other node reports the stopped peer down | **PASS** (expired at the 180 s handshake age; 80 s after stop) |
| 4 | Exact-layer diagnosis of failures | no product failures remain in the tested path |
| 5 | Cleanup: no namespaces, veth, TUN, processes, temp keys/images | PASS |

The task also required not to edit `docs/parity-matrix.md` or
`docs/plan-execution-status.md`; neither was touched. Nothing was committed.

## Topology and configuration

```
host
├── netns ozma-a                         netns ozma-b
│   veth-a 10.10.10.1/24  <──veth pair──>  veth-b 10.10.20.1/24
│   route 10.10.20.0/24 dev veth-a         route 10.10.10.0/24 dev veth-b
│   ozma0  10.200.1.1/24                   ozma0  10.200.1.2/24
│   route  10.200.1.2/32 dev ozma0         route  10.200.1.1/32 dev ozma0
│   WG UDP :51820                          WG UDP :51820
│   HTTP   :7382                           HTTP   :7383
└── ozma-node --profile vm --cap '' ...
```

Identities were fresh X25519 keypairs (Python `cryptography`, 32-byte private,
base64 in a mode-0600 file). The peer JSON used the new public keys:

```json
A pub: 0KPBWU6P/UQy87dYaFCJkvDv9Bq42ki2idJHtwmaPwE=
B pub: FxPDi5JFINCR0rCMcQyQUQjFMTR0Skf/ur8524sslnE=
```

The node invocation was the same shape as earlier runs, with no PATH shim, no
pre-created TUN, and no nft drop rules:

```bash
sudo ip netns exec ozma-a env HOME=... RUST_LOG=ozma_mesh=debug,ozma_node=info \
  target/debug/ozma-node \
  --name ozma-live-a --profile vm --cap '' \
  --http-port 7382 --hid-udp-port 7331 \
  --mesh --mesh-ip 10.200.1.1 --mesh-port 51820 \
  --mesh-key-file .../a.key \
  --mesh-peer '{"id":"ozma-live-b","wg_pubkey":"...","mesh_ip":"10.200.1.2","wg_port":51820}' \
  --mesh-dataplane --mesh-tun ozma0 --mesh-mtu 1420 --mesh-prefix-len 24
```

### Remaining harness workaround (not a new defect)

WireGuard still uses `peer.mesh_ip` as its UDP endpoint while the dataplane
routes that same address into the mesh TUN, so the encrypted UDP transport
would recurse into the TUN without scoping. As before, the test added
namespace-local policy routing:

```bash
# A (B mirrored)
ip route add 10.200.1.2/32 dev veth-a src 10.200.1.1 table 100
ip neigh add 10.200.1.2 lladdr <veth-b-mac> dev veth-a
ip rule add priority 100 ipproto udp sport 51820 lookup 100
sysctl -qw net.ipv4.conf.all.rp_filter=0
sysctl -qw net.ipv4.conf.veth-a.rp_filter=0
```

Overlay packets (ping/UDP to `10.200.1.2`) still follow the main-table
`/32 dev ozma0` route into the TUN, so the logged pump path is real. No nft
rules were installed; no TUN was pre-created.

## Fix evidence

### 1. TUN creation on iproute2 7.3

No harness workaround was used. The node logs the compatibility fallback and
brings `ozma0` up:

```
08:38:41.357847Z DEBUG ozma_mesh::dataplane: iproute2 rejected the explicit
  `persist` token; retrying with implicit persistence interface=ozma0
ozma0  UP  10.200.1.1/24 fe80::.../64      (A)
ozma0  UP  10.200.1.2/24 fe80::.../64      (B)
```

The first attempt is `ip tuntap add dev ozma0 mode tun persist`; on this
iproute2 the kernel still creates the TUN persistent with the implicit form
(`ip -d link` reports `persist on`). If both forms fail, the dataplane returns
the typed `mesh.tun-unsupported` error.

### 2. Non-overlay TUN traffic is dropped, not fatal

Both nodes' pump counters show the IPv6 router solicitation and IGMP/mDNS
traffic that previously tore the mesh down. The pump stayed `running` and the
mesh stayed available:

```json
A pump: {"state":"running",
  "tun_read_packets":7,"tun_read_bytes":404,
  "dropped_non_ipv4":5,"dropped_multicast":2,"dropped_broadcast":0,
  "dropped_no_route":0,
  "tun_read_errors":0,"tun_write_errors":0,
  "tunnel_send_errors":0,"tunnel_receive_errors":0}
```

The counts continued increasing as the host stack emitted more link-local
traffic (`dropped_non_ipv4` reached 9 on B), with no tunnel/dataplane teardown
and no error counter.

### Handshake

The first ICMP echo triggered the handshake; the remaining pings succeeded.
Both sides reported a completed boringtun handshake with a real handshake age:

```
A: peer ozma-live-b  tunnel=established  established=true
   last_handshake_age_secs=15  tx=420 rx=420
B: peer ozma-live-a  tunnel=established  established=true
   last_handshake_age_secs=15  tx=420 rx=420
```

### Ping through the overlay

```
$ sudo ip netns exec ozma-a ping -c 6 -i 0.5 -W 1 10.200.1.2
64 bytes from 10.200.1.2: icmp_seq=2 ttl=64 time=0.676 ms
... (seq 3..6) ...
6 packets transmitted, 5 received, 16.6667% packet loss, time 2520ms
rtt min/avg/max/mdev = 0.474/0.650/0.902/0.144 ms
```

The one lost packet is the handshake-triggering echo, as in the previous run.

### UDP echo through the overlay

```
reply echo:hello-0 from ('10.200.1.2', 9999)
reply echo:hello-1 from ('10.200.1.2', 9999)
reply echo:hello-2 from ('10.200.1.2', 9999)
```

### Dataplane and pump counters after traffic

Both status documents reported `state:"running"`, `available:true`, the
expected `dataplane` document, and real pump counters (A):

```
tun_read 15 pkts/1004 B   tun_to_tunnel 6/504   tunnel_received 5/420
tunnel_to_tun 5/420       dropped_tun_to_tunnel 0  dropped_tunnel_to_tun 0
dropped_non_ipv4 7        dropped_multicast 2      dropped_broadcast 0
dropped_no_route 0        all error counters 0
```

B reported the mirror counters. No counter was fabricated before traffic.

## Teardown and peer-down (now passing)

### Stopping node A

A was sent `SIGTERM`. The shutdown ordering now waits for the mesh task's
cooperative `MeshService::stop()` before any abort, so the dataplane teardown
ran:

```
08:40:02.495Z  INFO ozma_node: shutdown requested; stopping node tasks signal="SIGTERM"
08:40:02.509Z DEBUG ozma_mesh::dataplane: mesh dataplane stopped interface=ozma0
08:40:02.547Z  INFO ozma_node: Ozma node shutting down

$ ip -n ozma-a -br link show ozma0
Device "ozma0" does not exist.
$ ip -n ozma-a route | grep ozma0
(none)
```

`net.ipv4.ip_forward` remained `1`, but a fresh namespace on this host also
starts with `net.ipv4.ip_forward=1`, so the dataplane correctly treated it as a
pre-existing value that was not ours to restore. B's later `SIGTERM` also logged
`mesh dataplane stopped interface=ozma0` and left no `ozma0` or routes.

### Peer-down reporting

B polled every 10 s after A stopped. The handshake age tracked the real last
handshake, and the peer expired at the 180 s WireGuard reject time; because the
last successful handshake was already ~108 s old when A stopped, B reported the
peer down 80 s later:

```
08:40:48  established=true   tunnel=established  age=108  send_errors=0
08:41:08  established=true   tunnel=established  age=128  send_errors=0
08:41:38  established=true   tunnel=established  age=158  send_errors=0
08:41:58  established=true   tunnel=established  age=178  send_errors=0
08:42:08  established=false  tunnel=idle         age=null send_errors=0
```

After expiry, B still ran and kept its pump alive; the peer remained configured
for a future handshake:

```json
{"state":"running",
 "peer":{"id":"ozma-live-a","established":false,"tunnel":"idle",
         "last_handshake_age_secs":null},
 "pump":{"state":"running","dropped_non_ipv4":9,"dropped_multicast":2,
         "tunnel_send_errors":0,"tunnel_receive_errors":0}}
```

This replaces the previous failure where B still reported
`established:true` with a 362 s age after A had stopped.

## Exact layer outcomes

| Layer | Observation |
|---|---|
| Dataplane command | Explicit `persist` argv rejected by iproute2 7.3; implicit-persistence fallback succeeds and the TUN survives. Both forms rejected is `mesh.tun-unsupported`. |
| Pump ingress | IPv6 RS and IGMP/mDNS multicast are dropped into separate counters; `mesh.pump-no-route` drops are counted too; the pump never fails for them. Genuine tunnel/TUN errors still fail the pump. |
| Transport addressing | Still requires endpoint/overlay separation (policy route workaround above). Not changed by this fix set. |
| Shutdown | Cooperative wait precedes abort; `MeshService::stop()` deletes TUN/routes and restores owned forwarding state. |
| Liveness | boringtun timers are driven every second on the manager's receive loop; sessions expire and `last_handshake_age_secs` tracks the real handshake. |

## Cleanup

Stopped both nodes, then removed all namespace-local state:

* `ozma0` deleted in both namespaces by each node's own stop path;
* `ip netns del ozma-a` / `ozma-b` removed both namespaces and the veth pair;
* `/tmp/opencode/mesh-live2` removed (private keys, peer JSON, logs, evidence);
* verified on the host: no `ozma-a`/`ozma-b` namespaces, no `veth-a`/`veth-b`,
  no `ozma0`, no `ozma-node` process.

No host default route, DNS, firewall, or real interface was changed; all
link/route/rule/sysctl changes were inside `ozma-a`/`ozma-b` or the veth pair.

## Remaining gaps (unchanged by this fix set)

1. **Underlay endpoint and overlay identity are still not separated.** A peer
   needs an endpoint address distinct from its mesh IP, or route
   scoping/policy so encrypted UDP does not recurse into the mesh TUN.
2. **No automatic mDNS peer exchange/route reconciliation.** Peer routes still
   come from `--mesh-peer` and runtime `add_peer`.
3. **IPv4-only, single-queue pump.** No PMTU/ICMP handling, no NAT exercise
   here, no controller↔node mesh test.
4. **Session expiry is reported and driven, but no explicit reconnect policy
   beyond boringtun's own rekey/handshake attempts.**
