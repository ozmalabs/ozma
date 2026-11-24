# Phase 4 clean-host validation report

Date: 2026-11-25 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Validator: clean-host/Phase 4 subagent

## Verdict

**Gate 4 is not met.** The frozen tree builds, the full `cargo test
--workspace` suite is green, the bounded host and container harnesses pass, and
the Rust node serves health, HLS, snapshots, and QMP input against live QEMU
guests. Two Phase 4 requirements remain open:

1. Rust-node mDNS discovery does not reach the Python controller's zeroconf
   browser on this host. The live demo only registered after restarting both
   nodes with `--register-url`. Default `bash demo/run_demo.sh --rust` therefore
   starts nodes that the controller never sees.
2. WebRTC was not exercised; the node correctly returns a typed
   `503 webrtc-not-configured` without `--webrtc-helper`.

The privileged fresh-host install (system user, udev, AppArmor, libvirt) and the
Python-free install were not performed, and Python was intentionally not
deleted. See [Remaining Phase 4 blockers](#remaining-phase-4-blockers).

## Frozen commit

| Item | Value |
|---|---|
| HEAD before run | `72bad4ef48903aaef325c3f725a1a06f178626e6` |
| HEAD after run | `72bad4ef48903aaef325c3f725a1a06f178626e6` |
| Commit subject | `feat(phase3): console switching, USB/IP, clean-host, tiled EGL` |
| Concurrent commits during run | None |
| Build/test source | `git archive HEAD` → `/home/matt/ozma/phase4-frozen-72bad4ef48903aaef325c3f725a1a06f178626e6` |
| Freeze file | `/tmp/opencode/phase4-freeze-before.txt` |

The working tree was **not** clean and changed during the run from parallel
workstreams. At 07:09 only four routing/display docs were modified. By 07:17
`ozma-capture/src/dmabuf_egl.rs`, `ozma-node/src/hls.rs`,
`ozma-usb/src/lib.rs`, `ozma-usb/src/nbd.rs`, and new `ozma-usb` test files had
also appeared. No commit landed. Because the immutable archive was used for
every build and test, none of that work affected the results below. The archive
was deleted after the run; the commit hash is the reproducible reference.

## Environment

- Rust 1.95.0 (`cargo 1.95.0`, `rustc 1.95.0`), matching the container image.
- Rootless Podman 6.2.0 (`podman info` reports `Rootless: true`); Docker absent.
- Host tools: QEMU, virsh, ffmpeg, pactl, systemd-analyze present; shellcheck
  absent; avahi-daemon running.
- `/dev/kvm` present and writable; Alpine 3.21.3 virt ISO downloaded by the demo.
- The controller was run from a temporary venv
  (`/tmp/opencode/demo-venv`, Python 3.13.12) containing
  `controller/requirements.txt`. No global installs and no sudo were used.

## PASS / FAIL / SKIP matrix

| Check | Result | Evidence |
|---|---|---|
| `scripts/clean-host/run.sh --check-deps` (host) | **PASS** | 15 passed, 0 failed, 1 skipped (`shellcheck`). `libxdo` PASS after harness patch. |
| `scripts/clean-host/run.sh --container --full` | **PASS** | 15 passed, 0 failed, 1 skipped (`systemd-analyze` absent in image). |
| `cargo test --workspace` (frozen archive, container) | **PASS** | 137 suites: 1836 passed, 0 failed, 23 ignored. `ozma-node` lib: 304 passed. |
| `scripts/clean-host/run.sh --host` (bounded) | **PASS** | 17 passed, 0 failed, 0 skipped. `ozma-node --lib`: 301 passed, 3 ignored; `ozma-softnode`: 111 passed. |
| Synthetic desktop `/health` smoke | **PASS** | Container port 20004 and host port 21256 both returned `"ok":true`. |
| Binary `--help` / `--version` | **PASS** | `ozma-node`, `ozma-virtual-node`; `ozma-node 0.1.0`. |
| `scripts/systemd/verify-units.sh` (host) | **PASS** | 3/3 units (`ozma-node@`, `ozma-node-vm@`, `ozma-virtual-node.service`). |
| Container unit verification | **SKIP** | `systemd-analyze` is not installed in the build image; host run covers it. |
| `scripts/install-rust.sh --dry-run` | **PASS** | Exit 0; target prefix not created. |
| Real temp-prefix install | **PASS** | User layout under `/tmp/opencode/ozma-install-prefix`; both installed binaries run; 3 units and 4 env examples rendered. |
| `bash demo/run_demo.sh --rust` startup | **PASS** | Two Alpine/QEMU VMs, two Rust VM-profile nodes, Python controller; controller API ready. |
| Node health / display info | **PASS** | Both nodes `ok:true`, QMP capture source, `snapshot_available:true`. |
| Controller mDNS discovery of Rust nodes | **FAIL** | `/api/v1/nodes` empty before direct registration. `avahi-browse` and an independent python-zeroconf browser see no `_ozma._udp` record, including at node startup. A zeroconf self-test in the same environment is discovered normally. |
| Controller registration via `--register-url` | **PASS** | Both nodes appear with correct host/ports/capabilities; heartbeats accepted. |
| Controller scenario activation | **PASS** | `vm1`/`vm2` activate, `active_node_id` tracks the scenario. |
| HID routing (UDP → node → QMP) | **PASS** | `tests/test_e2e_switching.py --no-audio`: 24 PASS, including rapid switching. |
| Local HLS | **PASS** | Node `stream.m3u8` and `seg_*.ts` produced continuously. |
| Controller HLS proxy | **PASS** | `/api/v1/streams`, `/api/v1/streams/<node>/hls/stream.m3u8` (200), TS segment 200 `video/mp2t`. |
| Snapshot | **PASS** | Node and controller both return 200 `image/jpeg`, 1280×800. |
| QMP input | **PASS** | `POST /input/key` `{"keycode":30}` → 200 `{"ok":true,"sink":"qmp"}`; `/input/type` → 200, 14 events; controller `/api/v1/paste` → 200, 2 chars. |
| WebRTC | **SKIP** | `POST /webrtc/offer` and controller proxy return typed `503 webrtc-not-configured` (no helper). Not validated end-to-end. |
| Cleanup | **PASS** | Demo processes/VMs, pidfiles, QMP sockets, null sinks, venv, archive, and container image removed. |

## Harness fix found by this run

The first `--container --full` run failed at link time:

```text
error: linking with `cc` failed
rust-lld: error: unable to find library -lxdo
error: could not compile `ozma-hid` (test "inject_test")
```

`ozma-hid` depends on `enigo`, which links `libxdo`; the clean-host image did not
install it. This was a harness/environment gap, not product logic. Fixed in the
harness only:

- `scripts/clean-host/Containerfile`: added `libxdo-dev`.
- `scripts/clean-host/check-deps.sh`: added a `libxdo` required check and
  distro install hints (`libxdo-dev` / `libxdo-devel` / `xdotool`).
- `docs/clean-host.md`: added the dependency row.

With the rebuilt image the full suite passed. These edits are uncommitted in the
working tree, as instructed; the frozen archive was overlaid with the same
harness files for the rerun.

## Live demo detail

Default `/tmp/opencode/demo-venv` run, Rust backend, from the frozen archive:

```bash
export PYTHON=/tmp/opencode/demo-venv/bin/python
export OZMA_NODE_BIN=$ARCHIVE/target/clean-host/debug/ozma-node
PATH=/tmp/opencode/nosudo:$PATH bash demo/run_demo.sh --rust
```

- VMs: `vm1` (`/tmp/ozma-vm1.qmp`, VNC :5901), `vm2`
  (`/tmp/ozma-vm2.qmp`, VNC :5922), KVM accel.
- Nodes: `ozma-node --profile vm` on UDP 7332/7333, HTTP 7382/7383, QMP
  capture/input, HLS 15 fps, PipeWire monitor audio.
- Controller: Python `controller/main.py --virtual-only` on :7380.

The first launch produced the expected node health/HLS/snapshot/input responses,
but the controller node list stayed empty. Diagnostics:

- `scripts/clean-host/check-deps.sh` and `avahi-browse` show a working mDNS host.
- A python-zeroconf advertiser/browser pair for `_ozma._udp.local.` discovers
  itself immediately.
- `avahi-browse -rt _ozma._udp` saw nothing while a node was restarted, even
  during its startup announcement.
- Node logs report `mDNS advertising vm1._ozma._udp.local.` but also repeated
  `libmdns::fsm` parse warnings (`type 1388 is invalid`).

Restarting the same nodes with
`--register-url http://127.0.0.1:7380 --register-host 127.0.0.1` produced
`Registration accepted by controller`, populated `/api/v1/nodes`, and let all
remaining controller checks pass. This localises the failure to the Rust
`libmdns` advertisement/interoperability path (`ozma-node/src/mdns.rs`), not the
controller, the demo script, or the host network. It is product code and was
not modified by this task.

## Exact commands

```bash
# Freeze
git rev-parse HEAD
git archive HEAD | tar -x -C /home/matt/ozma/phase4-frozen-<hash>

# Harness (from the frozen archive)
scripts/clean-host/run.sh --check-deps
scripts/clean-host/run.sh --container --full --rebuild
scripts/clean-host/run.sh --host
scripts/systemd/verify-units.sh
scripts/install-rust.sh --dry-run --from-target target/debug --prefix /tmp/opencode/ozma-install-dryrun
scripts/install-rust.sh --from-target target/clean-host/debug --prefix /tmp/opencode/ozma-install-prefix

# Demo
PYTHON=/tmp/opencode/demo-venv/bin/python \
OZMA_NODE_BIN=$ARCHIVE/target/clean-host/debug/ozma-node \
PATH=/tmp/opencode/nosudo:$PATH \
    bash demo/run_demo.sh --rust
$VENV/bin/python tests/test_e2e_switching.py --no-audio
curl ... localhost:7380/api/v1/nodes
curl ... localhost:7382/display/snapshot
curl ... localhost:7380/api/v1/streams/<node>/hls/stream.m3u8
curl ... -X POST localhost:7382/input/key -d '{"keycode":30,"action":"tap"}'
```

Logs and artifacts were kept at `/tmp/opencode/phase4-artifacts/`
(`container-run.log`, `host-run.log`, `container-build.log`, `demo-start.log`,
`e2e-switching.log`, controller/node logs, a JPEG snapshot, an HLS playlist and
TS segment, and the dry-run/real install logs). Large build products were
removed.

## Remaining Phase 4 blockers

1. **Rust mDNS discovery (product).** Fix `ozma-node` mDNS so the controller's
   `zeroconf` browser discovers `_ozma._udp.local.` records, or make
   `--register-url` the documented/default demo path. Retest the default
   `bash demo/run_demo.sh --rust` with no direct-registration override.
2. **WebRTC (product/configuration).** Provide a GStreamer WebRTC helper and
   verify offer/answer, ICE, and a rendered frame. The typed 503 is correct but
   is not parity evidence.
3. **Fresh-host Python-free install.** The Rust installer only covers the node;
   the controller is still Python. Gate 4's "fresh-host install uses no Python"
   is unproven and cannot be claimed from this run.
4. **Privileged install assets.** System user creation, udev rules, the AppArmor
   drop-in, and libvirt `cgroup_device_acl` remain owned by
   `softnode/install/install.sh` and require root. They were not run (no sudo).
5. **Rollback tag.** `rust-cutover-<version>-python-rollback` was not created,
   because this task prohibited commits/tags.
6. **Python deletion.** `softnode/`, `node/`, `agent/`, and `src/` remain in
   place intentionally for the one-release overlap.
7. **Container parity for host-only checks.** The image has no
   `systemd-analyze`, QEMU, virsh, or pactl, so unit verification and the live
   demo are host-only. That is expected and reported as SKIP, not hidden.

No Gate 4 completion is claimed. The full Rust suite is green and the Rust node
is live-demo capable, but default discovery and WebRTC are not.
