# Phase 4 live demo rerun report

Date: 2026-11-25 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Validator: Phase 4 live-demo subagent
Frozen commit: `d2b83e923f692b0b15181c3c68302c90b7a0d3e8`
(`fix(node): mDNS discovery and graceful SIGTERM shutdown`)

## Verdict

The default `demo/run_demo.sh --rust` path now works end to end with no manual
registration. Both Rust VM-profile nodes are discovered through mDNS by the
unmodified Python controller without `--register-url`, and the demo passes
health, node list/detail, dashboard cards, per-node HLS, snapshots, QMP input,
paste, rapid scenario switching, graceful SIGTERM, and orphan cleanup.

This rerun resolves the two demo-scope gaps from
[`phase4-clean-host-report.md`](phase4-clean-host-report.md): default mDNS
discovery and the live Rust-node demo. It does **not** complete Phase 4. The
Python-free fresh-host install, the controller cutover, the privileged install
assets, the rollback tag, and Python-tree deletion remain open (see
[Remaining blockers](#remaining-phase-4-blockers)).

## Frozen source

| Item | Value |
|---|---|
| HEAD before run | `d2b83e923f692b0b15181c3c68302c90b7a0d3e8` |
| HEAD after run | `d2b83e923f692b0b15181c3c68302c90b7a0d3e8` |
| Tree | `a50f440070069f131591909e582b877d2d7da513` |
| Build source | `git archive HEAD` → `/home/matt/ozma/phase4-live-frozen-d2b83e9` |
| Build target | `/home/matt/ozma/phase4-live-target` (`cargo build -p ozma-node --bin ozma-node`, dev profile) |
| Freeze record | `/tmp/opencode/phase4-live-artifacts/freeze.txt` |
| Commits during run | None |

The checkout was **not** clean: four routing/display docs were modified before
the run, and a parallel workstream edited `ozma-usb`, `ozma-multiseat`,
`Cargo.toml`/`Cargo.lock`, and clean-host/unit scripts while the demo ran. Those
files are outside this task and were not touched. Every Rust build used the
immutable archive above, so the results correspond exactly to the frozen
commit. The controller pytest ran against the checkout's unchanged Python tree.

## Environment

- Rust 1.95.0 / cargo 1.95.0; Python 3.13.12 in `/tmp/opencode/phase4-live-venv`
  (controller requirements plus pytest/pytest-asyncio and Selenium).
- QEMU with KVM (`/dev/kvm` writable), ffmpeg n9.1, avahi-daemon active,
  PipeWire/PulseAudio reachable through `pactl`, Firefox available for headless
  dashboard verification.
- Route-level `/audio/*` validation (apply/remove/play/stop, null-sink monitor
  capture, controller proxy) was run separately against the host graph; see
  [`phase4-audio-live-report.md`](phase4-audio-live-report.md). The demo below
  used monitor audio but did not exercise these control routes.
- Two disposable direct-QEMU Alpine 3.21.3 VMs from `demo/start_vms.sh`
  (no libvirt domains, no sudo; a PATH `sudo` shim made `v4l2loopback` setup
  fail non-fatally).
- Ports: controller 7380, nodes 7332/7333 (UDP HID) and 7382/7383 (HTTP),
  VNC 5901/5922, QUIC 7384/7385.

## PASS / FAIL / SKIP matrix

| Check | Result | Evidence |
|---|---|---|
| Frozen HEAD unchanged | **PASS** | `git rev-parse HEAD` = `d2b83e9…` before and after; no commits |
| Build from frozen archive | **PASS** | `cargo build -p ozma-node --bin ozma-node` exit 0, `ozma-node 0.1.0` |
| Default demo startup (`--rust`) | **PASS** | 2 VMs, 2 Rust nodes, controller; controller API ready |
| mDNS discovery without `--register-url` | **PASS** | Controller log: `Node online: vm1… @ 192.168.40.19:7332`, `vm2… @ …:7333`; `/api/v1/nodes` lists both with `api_port`/`stream_url`; no registration URL was configured |
| mDNS advertisement independently visible | **PASS** | `avahi-browse -rt _ozma._udp` resolved a probe node at `192.168.40.19:17331` with `proto=1`, `cap=hid,video`, `api_port=17382`, `stream_port=17382` |
| Node persistence via re-query | **PASS** | Both nodes stayed in `/api/v1/nodes` for the full run (>2 min); controller re-resolved every 15 s with no heartbeats |
| Node health / connection | **PASS** | Both nodes `ok:true`; `profile=vm`, `sink_kind=qmp`, HLS `running`, input `available:true`, WebRTC `not-configured` |
| Node list / detail | **PASS** | `/api/v1/nodes` and `/api/v1/nodes/<id>` return host, HID port, API port, capabilities, audio type/sink, `stream_url` |
| Dashboard cards | **PASS** | Headless Firefox/Selenium: `node-count=2`, 2 `.node-card`s (`vm1` Active, `vm2` Ready), 2 scenario cards; screenshot `dashboard-loaded.png` |
| Local HLS per node | **PASS** | `stream.m3u8` 200, `#EXTM3U`, advancing media sequence; `seg_*.ts` 200 ≈100 KB |
| Controller HLS proxy | **PASS** | `/api/v1/streams/<id>/hls/stream.m3u8` 200; TS proxy 200 `video/mp2t` |
| Snapshot per node | **PASS** | `/display/snapshot` 200 `image/jpeg`, 1280×800, ≈96 KB each |
| Controller snapshot proxy | **PASS** | `/api/v1/streams/<id>/snapshot` 200 `image/jpeg`, 1280×800 |
| QMP input per node | **PASS** | `/input/key` `{"keycode":30}` → 200 `{"ok":true,"sink":"qmp"}`; `/input/type` → 200, 4 events; both nodes |
| Controller paste | **PASS** | `POST /api/v1/paste` → 200, `chars_sent=2` |
| Switching E2E | **PASS** | `tests/test_e2e_switching.py --no-audio`: 24 PASS, 0 FAIL, including HID routing to both nodes, QMP sockets, and five rapid switches |
| WebRTC — demo nodes | **PASS (typed refusal)** | Demo does not pass `--webrtc-helper`; node `/webrtc/offer` and controller `/api/v1/streams/<id>/webrtc` both return 503 `webrtc-not-configured` with remediation |
| WebRTC — helper path | **PASS** | Separate probe node (`--capture-provider fake`, `--webrtc-helper python3`, `gst_webrtc_worker.py`): `webrtc-client.py` reported `connection_state=connected`, `ice=completed`, 123 video buffers / 14,169,600 bytes of I420 320×240@15; no audio transceiver was offered |
| Graceful node SIGTERM | **PASS** | Both nodes exited in **102 ms**; logs show `shutdown requested … signal="SIGTERM"` and `Ozma node shutting down`; HLS ffmpeg children gone |
| Graceful WebRTC-probe SIGTERM | **PASS** | Probe node exited in 102 ms; `gst_webrtc_worker.py` child gone |
| Controller SIGTERM | **PASS** | Stop sequence logged orderly manager shutdown; demo pidfiles/sockets removed |
| Orphan processes / listeners | **PASS** | No `ozma-node`, controller, QEMU, ffmpeg, or helper processes after cleanup; no demo ports listening |
| Process-level shutdown test | **PASS** | `OZMA_NODE_SHUTDOWN_TEST=1 cargo test -p ozma-node --test shutdown -- --ignored` → 1 passed in 0.69 s |
| Gate 2 preflight (read-only) | **PARTIAL** | 13 PASS, 2 FAIL, 2 SKIP. FAILs are environmental: the node owns the single QMP client (a second greeting times out), and host ffmpeg lacks the VNC demuxer; the demo uses QMP capture |
| Gate 2 node/controller checks | **PASS** | 10 PASS, 0 FAIL, 2 SKIP (WebRTC not configured on demo node; controller input proxy opt-in) |
| Clean-host `--host` | **PASS** | 17 passed, 0 failed, 0 skipped; `ozma-node --lib` 312 passed/3 ignored; `ozma-softnode` 111 passed; synthetic `/health` pass |
| Controller pytest | **PASS** | `pytest tests/unit/controller -q`: 2496 passed, 7 skipped, 1 xfailed in 72.6 s |
| Fresh-host Python-free install | **SKIP / not attempted** | Out of scope for this run; still a Phase 4 blocker |
| Controller cutover | **SKIP / not attempted** | The unmodified Python controller is still the controller |
| Privileged install assets | **SKIP** | No sudo; not exercised |

## mDNS / discovery detail

Node startup, with no `--register-url`:

```text
INFO ozma_node: Resolved local IP ip=192.168.40.19
INFO ozma_node: mDNS advertising vm1._ozma._udp.local.
INFO ozma_node: mDNS advertising vm2._ozma._udp.local.
```

Controller startup and discovery:

```text
08:46:29 INFO ozma.discovery: mDNS browser started for _ozma._udp.local.
08:46:29 INFO ozma.discovery: Node online: vm1._ozma._udp.local. @ 192.168.40.19:7332 role=compute hw=x86_64
08:46:29 INFO ozma.discovery: Node online: vm2._ozma._udp.local. @ 192.168.40.19:7333 role=compute hw=x86_64
```

`/api/v1/nodes` then reported both nodes with `port` 7332/7333,
`api_port` 7382/7383, `capabilities=[hid,video,audio,power]`,
`audio_type=pipewire`, `audio_sink=ozma-vm1|ozma-vm2`, and
`stream_url=http://192.168.40.19:<api>/stream/stream.m3u8`. No direct
registration/heartbeat path was used; the controller re-resolved the mDNS
records every 15 s.

The independent probe confirmed the wire format after the fix:

```text
$ avahi-browse -rt _ozma._udp
= enp9s0 IPv4 webrtcprobe _ozma._udp local
   hostname = [SFFPC.local]
   address = [192.168.40.19]
   port = [17331]
   txt = ["role=compute" "proto=1" "cap=hid,video" "api_port=17382"
          "stream_port=17382" "stream_path=/stream/stream.m3u8" ...]
```

The known limits from the fix remain: libmdns never advertises loopback A
records (loopback-only development nodes), IPv6-only hosts fall back to all
interfaces, and multiple Rust nodes share the host SRV target and rely on
distinct instance names.

## Live demo detail

```bash
export PYTHON=/tmp/opencode/phase4-live-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/phase4-live-target/debug/ozma-node
PATH=/tmp/opencode/nosudo:$PATH bash demo/run_demo.sh --rust
```

- `vm1`: QMP `/tmp/ozma-vm1.qmp`, VNC :5901, node UDP 7332 / HTTP 7382.
- `vm2`: QMP `/tmp/ozma-vm2.qmp`, VNC :5922, node UDP 7333 / HTTP 7383.
- Controller: `controller/main.py --virtual-only` on :7380.

The first launch was externally killed mid-run (all demo processes disappeared
without shutdown log lines, while QEMU sockets stayed). The stack was relaunched
detached with `setsid`; all checks below are from that stable run. This was a
harness/process-supervision artefact, not a node/controller failure.

Switching E2E:

```text
$ python tests/test_e2e_switching.py --no-audio
[1] Both nodes online … PASS
[2] Scenarios defined … PASS
[3]/[4] Activate vm1/vm2 … PASS
[5]/[6] HID routing to vm1/vm2 … PASS
[7] QMP sockets … PASS
[8] Rapid switching (5×) … PASS
All tests passed.   (24 PASS, 0 FAIL)
```

Note: `test_e2e_switching.py` is a standalone script whose `test_*` helpers take
positional arguments, so it is run with the interpreter, not collected by
pytest. `pytest tests/test_e2e_switching.py --no-audio` is not a valid
invocation (unknown pytest option and required-argument fixtures).

Dashboard verification used headless Firefox with a wait for the WebSocket
snapshot plus initial API load:

```text
NODE_CARDS 2
CARD: vm1._ozma._udp.local. | vm1 | Active | 🔑 Token
CARD: vm2._ozma._udp.local. | vm2 | Ready | Switch | 🔑 Token
NODE_COUNT 2
SCENARIO_CARDS 2
```

## WebRTC detail

The demo launcher does not pass `--webrtc-helper`, so its nodes and the
controller proxy return the typed refusal:

```json
HTTP 503
{"ok":false,"code":"webrtc-not-configured",
 "error":"WebRTC is not configured on this node",
 "remediation":"Start ozma-node with --webrtc-helper …"}
```

A separate ephemeral node proved the helper path is functional:

```bash
ozma-node --name webrtcprobe --profile vm \
  --capture-provider fake --fake-width 320 --fake-height 240 --fake-fps 15 \
  --input-provider recording --cap hid,video \
  --hid-udp-port 17331 --http-port 17382 \
  --webrtc-helper python3 \
  --webrtc-helper-script softnode/gst_webrtc_worker.py \
  --webrtc-socket /tmp/opencode/webrtcprobe/webrtc.sock \
  --webrtc-shm /tmp/opencode/webrtcprobe/frame.bgra \
  --webrtc-width 320 --webrtc-height 240 --webrtc-fps 15

python3 scripts/gate2/webrtc-client.py --node-url http://127.0.0.1:17382 --duration 8
```

Result: `ok:true`, offer answered, ICE `completed`, DTLS connected, 123 video
buffers / 14,169,600 bytes, caps
`video/x-raw, format=I420, width=320, height=240, framerate=15/1`. Audio was not
offered, so this is video-path evidence only. The demo itself still does not
wire the helper; that is a demo/launcher gap, not a node defect.

## Shutdown detail

`SIGTERM` to both demo nodes:

```text
shutdown requested; stopping node tasks signal="SIGTERM"
Ozma node shutting down
```

Exit to process disappearance: 102 ms for both nodes. Their ffmpeg children
were gone, and no node/helper/ffmpeg/controller/QEMU processes remained. The
checked-in process test passed independently:

```text
test sigterm_exits_the_node_within_the_bound_and_reaps_children ... ok
test result: ok. 1 passed; 0 failed; 0 ignored
```

## Gate 2 harness note

`scripts/gate2/run-gate2.sh` was run read-only against the live demo with
`OZMA_GATE2_SKIP_LIBVIRT=1`. Node/controller checks passed 10/10. The preflight
reported two failures that are expected in this topology:

1. QMP greeting timeout — the running node owns the single QMP control client;
   a second probe cannot complete the handshake.
2. No ffmpeg VNC demuxer on this host — the demo uses QMP capture, not VNC.

Neither is a demo/controller regression. The report log is preserved under the
artifacts directory.

## Artifacts

`/tmp/opencode/phase4-live-artifacts/` contains:

- `freeze.txt`, `build.log`, `demo-start.log`, `demo-start2.log`
- `demo-logs/` (controller, both nodes, both VM serial logs)
- `e2e-switching.log`, `e2e-switching2.log`
- `playlist-7382.m3u8`, `playlist-7383.m3u8`, `seg-7382.ts`, `seg-7383.ts`
- `ctrl-playlist.m3u8`, `ctrl-seg.ts`, `snapshot-7382.jpg`, `snapshot-7383.jpg`,
  `ctrl-snapshot.jpg`
- `webrtc-7382.json`, `webrtc-7383.json`, `ctrl-webrtc.json`
- `webrtcprobe.log`, `webrtc-client.log`, `webrtc-client-report.json`
- `dashboard.png` (initial load), `dashboard-loaded.png`, `selenium.log`
- `gate2/` report, `gate2-run.log`
- `clean-host/` log, `clean-host-host.log`
- `controller-pytest.log`, `shutdown-test.log`

Large build targets, the frozen extraction, the venv, and the disposable VMs
were removed after the run.

## Remaining Phase 4 blockers

1. **Fresh-host Python-free install is unproven.** The Rust installer covers
   `ozma-node`/`ozma-virtual-node` only; the controller is still Python and the
   end-to-end demo requires it.
2. **Controller cutover has not happened.** No Rust controller replacement or
   rollback tag (`rust-cutover-<version>-python-rollback`) was produced; this
   task prohibited commits/tags.
3. **Privileged install assets remain Python-owned.** System user creation,
   udev rules, the AppArmor drop-in, libvirt `cgroup_device_acl`, and
   `v4l2loopback` setup need root and were not run.
4. **WebRTC in the demo is not wired.** The helper path works in a separate
   probe, but `demo/run_demo.sh --rust` does not pass `--webrtc-helper`, so its
   nodes return the typed 503. WebRTC audio was not exercised.
5. **Python node trees still exist** (`softnode/`, `node/`, `agent/`) for the
   one-release overlap; deleting them is a post-cutover step.
6. **Known mDNS limits** remain: loopback-only nodes, IPv6-only hosts, shared
   SRV target across Rust nodes (distinct instance names still work).
