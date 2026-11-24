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

The privileged fresh-host install (system user, udev, AppArmor, libvirt) is now
scripted and rootlessly verified but was not performed on this host, and the
Python-free install was not performed; Python was intentionally not deleted.
See [Follow-up: system install surface](#follow-up-system-install-surface-2026-11-25)
and [Remaining Phase 4 blockers](#remaining-phase-4-blockers).

> **Follow-up (2026-11-25):** blocker 1, Rust-node mDNS discovery, is fixed.
> Root cause, fix, and live browser/controller evidence are in
> [Follow-up: Rust mDNS discovery fix](#follow-up-rust-mdns-discovery-fix-2026-11-25).
>
> **Live demo rerun (2026-11-25):** the default `--rust` demo now discovers both
> nodes by mDNS and passes switching, HLS, snapshot, input, dashboard, and
> shutdown checks. See
> [`phase4-live-demo-report.md`](phase4-live-demo-report.md). WebRTC's helper
> path was proven with a separate probe, but the demo does not wire the helper
> and still returns the typed 503. Phase 4 itself remains incomplete.

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
| Controller mDNS discovery of Rust nodes | **FAIL → FIXED** (follow-up, see below) | At run time: `/api/v1/nodes` empty before direct registration. `avahi-browse` and an independent python-zeroconf browser see no `_ozma._udp` record, including at node startup. A zeroconf self-test in the same environment is discovered normally. Follow-up (2026-11-25): after the `ozma-node/src/mdns.rs` fix, avahi and python-zeroconf resolve the Rust advertisement at `192.168.40.19:17331`; the controller's real `DiscoveryService` adds `mdnsprobe-final._ozma._udp.local.` with `api_port=17382`. |
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

## Follow-up: Rust mDNS discovery fix (2026-11-25)

### Root cause

`ozma-node/src/mdns.rs` passed the fully-qualified constant
`SERVICE_TYPE = "_ozma._udp.local."` to
`libmdns::Responder::register`. `libmdns` 0.9.1 appends `.local` itself
(`format!("{}.local", svc_type)` and
`format!("{}.{}.local", svc_name, svc_type)`), so the registered names became:

```text
type:     _ozma._udp.local..local
instance: <name>._ozma._udp.local..local
```

`libmdns`'s DNS name writer splits on `.` and emits an empty length byte for
the empty label (`local..local`). A DNS-SD parser stops at that zero byte, so
the RR's type/class/TTL bytes were read from the trailing `local` text. The
first two bytes `0x05 0x6c` decode as type 1388 (0x056c), which is exactly the
warning the node logged:

```text
WARN libmdns::fsm: couldn't parse packet from 192.168.40.19:5353: type 1388 is invalid
```

Every announced PTR/SRV/TXT record was malformed. `avahi-browse` and
python-zeroconf discarded the packets, so the controller discovered nothing.
The controller, `_ozma._udp.local.` typing, and the host were not at fault.

### Fix

- Pass the unqualified `_ozma._udp` string to `libmdns`; the public
  `SERVICE_TYPE` stays `_ozma._udp.local.` for logs/registration.
- Add `mdns::advertise_on(name, port, txt, Some(ip))`, which uses
  `Responder::new_with_ip_list` to publish only the address the node resolved
  (or `--register-host`). Without this, `libmdns` published every non-loopback
  interface and avahi could resolve the libvirt bridge (`192.168.122.1`)
  instead of the LAN address (`192.168.40.19`); the controller takes
  `parsed_addresses()[0]`.
- Reject TXT `key=value` entries over 255 bytes with a typed error before
  calling `libmdns`, which otherwise panics on them.
- The node still advertises `hid_udp_port` as the SRV port and keeps
  `api_port`/`stream_port`/`stream_path` in TXT, matching `node.py` and
  `soft_node.py`.

### Live evidence

Before the fix, with `ozma-node --name mdnsprobe-before --profile vm
--capture-provider fake --input-provider recording --hid-udp-port 17331
--http-port 17382 --cap hid` and no `--register-url`:

```text
avahi-browse -rt _ozma._udp                  → no output
python-zeroconf ServiceBrowser              → TOTAL 0
node log                                    → libmdns ... type 1388 is invalid
```

After the fix (`ozma-node --name mdnsprobe-evidence ... --cap hid,video`):

```text
$ avahi-browse -rt _ozma._udp
+ enp9s0 IPv4 mdnsprobe-evidence    _ozma._udp    local
+ enp9s0 IPv6 mdnsprobe-evidence    _ozma._udp    local
   hostname = [SFFPC.local]
   address = [192.168.40.19]
   port = [17331]
   txt = ["proto=1" "role=compute" "hw=x86_64" "cap=hid,video"
          "api_port=17382" "stream_port=17382" ...]

$ python3 zeroconf ServiceBrowser(_ozma._udp.local.)
Added mdnsprobe-evidence._ozma._udp.local.
 type _ozma._udp.local. name mdnsprobe-evidence._ozma._udp.local.
 server SFFPC.local. port 17331
 addrs ['192.168.40.19'] props {b'proto': b'1', b'cap': b'hid,video',
 b'api_port': b'17382', b'stream_port': b'17382', ...}
```

The controller's own discovery code (`controller/discovery.py`) was then run
in-process against the same live advertisement:

```text
NODE mdnsprobe-evidence._ozma._udp.local. 192.168.40.19 17331 compute x86_64
     ['hid', 'video'] 17382 17382
```

Finally, the checked-in opt-in regression test passed against a fresh
advertisement:

```text
$ OZMA_MDNS_LIVE=1 cargo test -p ozma-node --test mdns_live -- --ignored --nocapture
test python_zeroconf_browses_the_rust_advertisement ... ok
```

### Remaining mDNS gaps

- `libmdns` always skips loopback interfaces, so a loopback-only node (the
  soft-node development shape) still gets no A record. The main node path
  resolves a LAN address and is unaffected.
- The advertisement is IPv4-filtered when a local IPv4 is known. IPv6-only
  hosts fall back to all interfaces and the controller does not bracket IPv6
  literals when building URLs.
- `libmdns` advertises the host's `hostname.local.` rather than a per-node
  hostname; multiple Rust nodes share one SRV target and rely on distinct
  instance names.
- No checked-in test covers multi-interface selection or avahi goodbye
  behaviour; both were inspected manually.

## Follow-up: system install surface (2026-11-25)

The privileged fresh-host install is now scripted and rootlessly verified. It
has still not been run on a real host, and no sudo was used by this work. The
2026-11-25 follow-up (script fixes plus host/container reruns) is in
[`phase4-system-install-report.md`](phase4-system-install-report.md).

| Surface | Status | Evidence |
|---|---|---|
| `scripts/install-system.sh` | added | Creates the `ozma-node` user/group (existing groups only, per hardware/vm/desktop profile), state/runtime/config dirs, units and env examples, udev rules, AppArmor drop-in, tmpfiles/modules-load snippets, and a copy-only libvirt snippet. Supports `--dry-run`, `--prefix`, `--root`, `--force`, `--usb-gadget`, `--no-user`, and `--no-*` toggles. Never invokes sudo, enables/starts a unit, or edits `qemu.conf`. |
| udev assets | added | `scripts/system/udev/70-ozma-node-{uinput,input,hidg,v4l2,render,gadget}.rules.in`; selected by profile; `udevadm verify -N never` PASS for all six. |
| AppArmor | added | `scripts/system/apparmor/libvirt-qemu.d-ozma` installed to the libvirt-qemu abstraction directory. Syntax check is SKIP on this host (`apparmor_parser` absent), not a failure. |
| libvirt | copy-only | `scripts/system/libvirt/qemu-cgroup_device_acl.conf.snippet` installed to `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf`; merging into `/etc/libvirt/qemu.conf` remains an operator step. |
| Verification | added | `scripts/verify-system-install.sh`: 77 passed, 0 failed, 1 skipped (host; 75/0/3 in a minimal rootless container). Renders all profiles, checks dry-run/force/idempotence/modes/profile groups/tmpfiles/secrets/world-writable paths, validates udev syntax, and runs system+user unit verification. See [`phase4-system-install-report.md`](phase4-system-install-report.md). |
| Unit verification | extended | `scripts/systemd/verify-units.sh` now verifies 3 system units and 3 least-privilege user units (PASS, systemd 262). |
| Clean-host harness | extended | `scripts/clean-host/in-container.sh` runs the rootless system verification; `check-deps.sh` reports `udevadm`, `apparmor_parser`, and `systemd-tmpfiles` as optional tools. |

Operator/root-only steps that remain:

1. Run `sudo scripts/install-system.sh --profile ...` on a fresh host (not done
   here; this checkout is not root).
2. Merge `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf` into
   `/etc/libvirt/qemu.conf`, restart libvirtd, and restart VMs.
3. Apply the printed udev/tmpfiles/AppArmor reloads and enable/start the
   selected unit. Exact commands are in
   `/etc/ozma/install-operator-commands.txt`; the installer never runs them.
4. USB gadget: mount ConfigFS, provide a hardware UDC, and arrange writable
   gadget ownership. The node does not get `CAP_SYS_ADMIN`.
5. Desktop: prefer a systemd `--user` unit; a system unit cannot see a logged-in
   user's PipeWire session by default.
6. QMP sockets for the VM profile: use the Rust supervisor's opt-in
   `--provision` path or an existing libvirt hook. The installer does not
   create QMP sockets or edit domain XML.

No Gate 4 claim changes: a Python-free fresh-host install and the privileged
host run are still outstanding.

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

1. **Rust mDNS discovery (product) — fixed 2026-11-25.** The malformed
   `libmdns` service type and multi-interface address selection are corrected;
   avahi, python-zeroconf, and the controller `DiscoveryService` now see the
   Rust node without `--register-url`. See
   [Follow-up: Rust mDNS discovery fix](#follow-up-rust-mdns-discovery-fix-2026-11-25)
   for evidence and the remaining IPv6/loopback limits.
2. **WebRTC (product/configuration).** Provide a GStreamer WebRTC helper and
   verify offer/answer, ICE, and a rendered frame. The typed 503 is correct but
   is not parity evidence.
3. **Fresh-host Python-free install.** The Rust installer only covers the node;
   the controller is still Python. Gate 4's "fresh-host install uses no Python"
   is unproven and cannot be claimed from this run.
4. **Privileged install assets.** System user creation, udev rules, the AppArmor
   drop-in, and the libvirt `cgroup_device_acl` snippet are now scripted by
   `scripts/install-system.sh` and rootlessly verified by
   `scripts/verify-system-install.sh`; the privileged run on a real host remains
   outstanding. `softnode/install/install.sh` is no longer the only owner of
   these steps.
5. **Rollback tag.** `rust-cutover-<version>-python-rollback` was not created,
   because this task prohibited commits/tags.
6. **Python deletion.** `softnode/`, `node/`, `agent/`, and `src/` remain in
   place intentionally for the one-release overlap.
7. **Container parity for host-only checks.** The image has no
   `systemd-analyze`, QEMU, virsh, or pactl, so unit verification and the live
   demo are host-only. That is expected and reported as SKIP, not hidden.

No Gate 4 completion is claimed. The full Rust suite is green and the Rust node
is live-demo capable, but default discovery and WebRTC are not.

## Live demo rerun (2026-11-25)

A rerun from frozen commit `d2b83e923f692b0b15181c3c68302c90b7a0d3e8` used
`git archive` + a fresh `cargo build -p ozma-node` and the two disposable
Alpine/QEMU demo VMs. Results:

- Default mDNS: **PASS**. Both Rust nodes appeared in the unmodified controller
  without `--register-url` (controller log and `/api/v1/nodes`), and an
  independent `avahi-browse` resolved the corrected `_ozma._udp` record.
- Live demo: **PASS**. Health, node detail, dashboard cards (2 node cards,
  2 scenario cards in headless Firefox), per-node HLS, snapshots, QMP input,
  paste, and `tests/test_e2e_switching.py --no-audio` (24 PASS) all passed.
- WebRTC: **PASS for the helper path** in a separate fake-capture probe
  (`webrtc-client.py`: connected, ICE completed, 123 video buffers, I420
  320×240). The demo launcher still does not pass `--webrtc-helper`, so demo
  nodes correctly return 503 `webrtc-not-configured`.
- Shutdown: **PASS**. SIGTERM exited both nodes in 102 ms with no surviving
  ffmpeg/helper children; the checked-in process test also passed.
- Harnesses: clean-host `--host` 17/17 PASS, controller pytest 2496 passed.

Full matrix and artifacts: [`phase4-live-demo-report.md`](phase4-live-demo-report.md).
The Python-free install, controller cutover, privileged assets, rollback tag,
and Python-tree deletion remain blockers; Gate 4 is still not complete.
