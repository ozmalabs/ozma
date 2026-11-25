# Frozen one-release-overlap gates at `6b881a1`

Date: 2026-11-25 (Australia/Sydney; UTC in logs below)
Baseline: `6b881a1539c7fd2b0671e89d8e1f7c19daae5d82`
(`feat(phase4): VNC Tight, libvirt URI/ACPI fixes, audit refresh 2`)
Validator: Phase 4 overlap-gate subagent
Checkout: detached worktree `/tmp/opencode/ozma-overlap-6b881a1`
Primary checkout: **not used** (it holds unrelated display/AIDA64 work)

## Verdict

**The commit is a viable one-release-overlap candidate, but it is not
taggable as-is.** Three of the seven audit §17 P0 preconditions are now
discharged at this exact commit (frozen evidence, container gate, live
`ozma-virtual-node` supervisor). Two remain unchanged by design because the
Python trees stay in place for the overlap (Python-only providers and
remaining CI/build/test references). The remaining hard blockers are:

1. `demo test` is still **70 passed / 5 failed** — the same five
   controller-only assertions. The release owner recorded waiver **W1** for
   them on 2026-11-25.
2. The rollback tag **`rust-cutover-0.1.0-python-rollback`** was created on the
   frozen code commit `6b881a1` after this evidence was reviewed.

Python-tree deletion remains **NO-GO** independently of the overlap tag.

**No product code changed. No commit and no tag were created.** The only
repository writes are this file and the appended section in
`docs/phase4-live-cutover-report.md`. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were **not** edited.

## 1. Workspace and metadata

All commands ran in the detached worktree with
`CARGO_TARGET_DIR=/home/matt/ozma/ozma-overlap-target`.

| Check | Result | Exact evidence |
|---|---|---|
| `cargo metadata --locked --format-version 1` | **PASS** | exit 0; **3,357,633-byte** JSON; `Cargo.lock` not rewritten. |
| `cargo test --workspace --no-fail-fast` | **PASS** | **2,755 passed, 0 failed, 51 ignored** across **206** result blocks; exit 0. |
| `cargo check --workspace --all-targets` | **PASS** | exit 0 in **33.76 s**. One `ozma-node` lib warning: pre-existing `unused_mut` at `ozma-node/src/controller_compat.rs:4815`. |
| `cargo fmt --check -p ozma-node -p ozma-power` | **PASS** | exit 0 on both crates touched by `6b881a1`. |
| `bash scripts/systemd/verify-units.sh` | **PASS** | **8/8** (`system` and `user` for `ozma-node@`, `ozma-node-vm@`, `ozma-virtual-node`, `ozma-multiseat`). |
| Worktree cleanliness | **PASS** | `git status --porcelain` empty before the run; test-generated controller state was removed afterwards; `Cargo.lock` unchanged. |

The workspace failure count is zero; the test summaries contain no `FAILED`,
panic, or compile error. The `ozma-node` lib test block alone reported
**567 passed / 4 ignored**.

## 2. Container clean-host gate

The image was built once with network enabled (`--network=host`), then the
gate ran rootless Podman 6.2.0 with the frozen worktree bind-mounted.

```bash
podman build --network=host -t localhost/ozma-clean-host:1.95 \
    -f scripts/clean-host/Containerfile scripts/clean-host
OZMA_CLEAN_HOST_TARGET=/home/matt/ozma/ozma-overlap-clean-target \
OZMA_CLEAN_HOST_LOG_DIR=/tmp/opencode/ozma-clean-logs \
    scripts/clean-host/run.sh --container --full
```

| Item | Value |
|---|---|
| Container runtime | rootless Podman 6.2.0 (`Rootless: true`) |
| Image build | **PASS** (`Successfully tagged`, exit 0) |
| Image ID | `36d846efd466ab6d2a881710657687d8454d50651d8b5976133da833391ff14a` |
| Local manifest digest | `sha256:2bd1b59e0747f3817a29410f50d256c5227248d528705b44c87d026400bbed6f` |
| Image size | 2,483,390,619 bytes |
| Dependency report | **11 passed / 0 failed / 8 skipped** (skips: qemu, virsh, pactl, systemd-analyze, udevadm, apparmor_parser, systemd-tmpfiles, shellcheck) |
| Build | `cargo build -p ozma-node -p ozma-softnode -p ozma-multiseat --bins` **PASS**, 58.05 s |
| Full test selection | `cargo test --workspace` **PASS**: **2,755 passed / 0 failed / 51 ignored** (206 blocks), 2 m 37 s |
| `ozma-node --lib` | **567 passed / 4 ignored** |
| `ozma-virtual-node` | **139 passed** |
| Binary/help, shell syntax, install dry-run, system-install surface | all **PASS** |
| Startup smoke | **PASS** — desktop profile answered `/health` on port 20004 |
| Harness summary | **20 passed / 0 failed / 1 skipped** (`systemd-analyze` absent in image) |
| Final line | `RESULT: PASS` |
| Cleanup | container `--rm`; image and target removed after evidence capture (see §7) |

No environmental blocker remained once the image build was given network
access. This discharges §17 P0 6 at `6b881a1`.

## 3. Default Rust demo

### 3.1 Environment isolation (required on this host)

The literal host invocation
`PYTHON=/tmp/opencode/overlap-venv/bin/python OZMA_NODE_BIN=... bash demo/run_demo.sh`
failed twice for **environmental** reasons, not product reasons:

1. **Controller state directory permissions.** The first attempt died before
   serving HTTP because a prior privileged install created
   `/var/lib/ozma` as `ozma-node:ozma-node 0750`; the demo controller runs as
   the invoking user and got
   `PermissionError: [Errno 13] Permission denied: '/var/lib/ozma/dns_filter_state.json'`.
2. **External mDNS name collision.** With that path bind-mounted to a writable
   directory, the controller raised `zeroconf._exceptions.NonUniqueNameException`
   because another host on the LAN (`192.168.40.215:7381`) already advertises
   `Ozma Grid._ozma-grid._tcp.local.`; the demo's fixed grid name is
   `"Ozma Grid"`.

Both are host/network facts. To validate the product without editing it, the
demo ran in a disposable network namespace joined to the host by a veth pair,
with a private writable `/var/lib/ozma` bind mount:

```bash
sudo ip netns add ozma-demo
# veth-ozma (host 10.203.0.1/24) <-> veth-ozma-ns (netns 10.203.0.2/24)
sudo ip netns exec ozma-demo ip route add 224.0.0.0/4 dev veth-ozma-ns
sudo ip netns exec ozma-demo ip route add default via 10.203.0.1
sudo ip netns exec ozma-demo runuser -u matt -- env ... \
  bwrap --dev-bind / / --bind /tmp/opencode/overlap-var-ozma /var/lib/ozma \
    env PYTHON=/tmp/opencode/overlap-venv/bin/python \
        OZMA_NODE_BIN=/home/matt/ozma/ozma-overlap-target/debug/ozma-node \
        bash demo/run_demo.sh
```

No host file or firewall rule was changed; `/var/lib/ozma` remained
`ozma-node:ozma-node 0750` before and after.

### 3.2 Live checks

| Check | Result | Evidence |
|---|---|---|
| Backend | **PASS** | `Backend: rust (default: rust)`; no `--rust`, no `--python`. |
| Discovery | **PASS** | Controller `/api/v1/status` listed `vm1._ozma._udp.local.` (7332, HTTP 7382) and `vm2._ozma._udp.local.` (7333, HTTP 7383); logs `Node online: vm1…`, `Node online: vm2…`. |
| Health | **PASS** | Both node `/health` returned `{"ok":true,…}`; controller answered `/api/v1/status`. |
| HLS | **PASS** | `GET :7382/stream/stream.m3u8` returned a live `#EXTM3U` playlist (`MEDIA-SEQUENCE:26`, 1 s segments); `seg_*.ts` files present for both VMs. |
| Snapshot | **PASS** | `GET :7382/display/snapshot` → 200 `image/jpeg`, **96,470 bytes**, decoded **1280×800 RGB**. |
| Input | **PASS** | `POST :7382/input/key {"keycode":30,"down":true/false}` → `{"ok":true,"sink":"qmp"}` twice; post-injection snapshot differed in a 71-pixel region (bbox `136,624–144,640`), proving real QMP injection. |
| Switching | **PASS** | `POST /api/v1/scenarios/vm2/activate` → active scenario `vm2`, active node `vm2._ozma._udp.local.`; then `vm1` → active scenario `vm1`, active node `vm1._ozma._udp.local.`. |
| Shutdown | **PASS** | `bash demo/run_demo.sh stop` killed controller PID 643283, nodes 643278/643281, QEMU 643231/643233; afterwards no matching processes, no `/tmp/ozma-vm*.qmp`, no pidfiles, and ports 7380/7332/7333/7382/7383/5901/5922 free. |

### 3.3 `demo test`

`bash demo/run_demo.sh test` (Rust backend) exited 1. The core suite passed
completely (`All tests passed.`): discovery, both node ports, scenarios,
bindings, activation, HID packets, QMP sockets, five rapid switches, audio
type/sink, monitor source/link state, and WirePlumber-managed mic routing.

The extended suite ended **70 passed, 5 failed, 0 skipped**. Exact failures:

```text
[F19] Macro recording and playback
  FAIL  GET /api/v1/macros returns data
  PASS  macro recording started
  PASS  macro recording stopped
[F20] Automation script execution
  FAIL  automation script accepted
[F25] Ozma Connect status
  FAIL  AI setups used: 0
[F26] Ozma Connect feature gating
  FAIL  noise_cancellation blocked on free tier
  FAIL  reason mentions upgrade:
```

These match the known set: three controller-only Ozma Connect assertions
(`AI setups used`, `noise_cancellation blocked`, `reason mentions upgrade`)
plus the macro/automation environment failures F19/F20. No node, HLS, VNC,
snapshot, `/video`, input, or switching assertion failed.

Cleanup on the failing suite still ran: `Stopping Ozma demo...`, controller and
both nodes killed, both QEMU VMs stopped, no listeners or processes left.

A first attempt without `pytest` in the demo venv failed the extended script at
import time (`ModuleNotFoundError: No module named 'pytest'`); this was a
validator-environment gap, not a product failure. Installing `pytest` and
`pytest-asyncio` into the disposable venv and setting `PYTHONPATH` to the
worktree produced the 70/5 result above. The demo venv is not a product
artifact.

## 4. `ozma-virtual-node` systemd supervisor

A disposable libvirt domain and a transient systemd unit with the packaged
unit's service semantics were used. No fake source was involved.

### 4.1 Fixture

- ISO: Alpine `alpine-virt-3.24.4-x86_64.iso`
  (`sha256:157d353f8e54252cad2d9812a0d56bb7fa2231fc154a110e1d180d7480ef8b4f`),
  installed at `/var/lib/libvirt/images/ozma-overlap-alpine.iso`.
- Domain `ozma-overlap-gate`: KVM, 512 MiB, 1 vCPU, live CD-ROM, user-mode NIC,
  VNC `127.0.0.1:5909`, plus a libvirt `qemu:commandline` QMP chardev
  `socket,id=ozma-mon,path=/tmp/ozma-overlap-vm.qmp,server=on,wait=off`.
- The domain was defined/started with `virsh` against `qemu:///system`.

### 4.2 Unit

```text
systemd-run --unit=ozma-overlap-virtual-node \
  --property=Type=simple \
  --property=User=ozma-node --property=Group=ozma-node \
  --property='SupplementaryGroups=libvirt kvm input video render' \
  --property=TimeoutStopSec=30 --property=KillMode=control-group \
  --property=Restart=no \
  --setenv=LIBVIRT_DEFAULT_URI=qemu:///system \
  /tmp/opencode/overlap-bin/ozma-virtual-node \
    --node-bin /tmp/opencode/overlap-bin/ozma-node \
    --status-port 7390 --child-poll-interval-ms 1000 \
    --base-port 7432 --node-http-base 7482
```

The packaged `scripts/systemd/ozma-virtual-node.service` was also rendered and
verified (`PASS system`/`PASS user` in `verify-units.sh`). The transient unit
keeps its `Type=simple`, unprivileged `ozma-node` account, group set, 30 s stop
timeout, and control-group kill semantics; `LIBVIRT_DEFAULT_URI` was added
because a non-root `virsh` otherwise defaults to `qemu:///session`.

### 4.3 Results

| Check | Result | Evidence |
|---|---|---|
| Discover VM | **PASS** | `/status` → `discovered:1, tracked:1, running:1, failed:0, skipped:0`; VM `ozma-overlap-gate`, QMP `/tmp/ozma-overlap-vm.qmp`. |
| Spawn child | **PASS** | Supervisor PID 584534; child PID 584652, PPID 584534, user `ozma-node`. |
| Expected argv | **PASS** | Exact `child_argv` and `/proc/584652/cmdline`: `ozma-node --profile vm --name ozma-overlap-gate --qmp-socket /tmp/ozma-overlap-vm.qmp --hid-udp-port 7432 --http-port 7482 --capture-provider qmp --input-provider qmp --power-provider qmp`. |
| Status | **PASS** | Supervisor `/health` `ok:true, running:1`; child `/health` `ok:true`; child `/power/state` `{"available":true,"status":"running"}`; polled child `reachable:true`, input `qmp/available/injecting`, power `running`. |
| Stop/reap | **PASS** | `systemctl stop` → `inactive`, MainPID 0, `Result=success`; journal: `Shutdown signal received`, `Shutting down: stopping every per-VM ozma-node child...`, `Stopping ozma-node for VM ozma-overlap-gate`, child `SIGTERM`, `Ozma node shutting down`, `Deactivated successfully`; no child/supervisor process and ports 7390/7432/7482 free. |
| No fake success | **PASS** | Child was a real `ozma-node` bound to a real QMP socket; the state came from QMP (`power running`, `input qmp`), not a stub. |

Operational findings (not gate failures):

- A system-domain QMP monitor socket is created `0775` owned by
  `libvirt-qemu`. The test account is in `libvirt` but not `libvirt-qemu`, so
  this run applied a test-only `chmod 0666` to the disposable socket. The
  packaged unit/config does not solve this; document it as an operator
  requirement or ship a permission mechanism.
- For system domains, `LIBVIRT_DEFAULT_URI=qemu:///system` (or an equivalent)
  must be present in the supervisor environment; the unit does not set it.

## 5. Audit §17 P0 decision table

Status vocabulary: **PASS** = verified at `6b881a1`; **FAIL** = gate still
red; **N/A (overlap)** = deliberately not required while the Python trees
remain; **Waiver** = proposed, not yet formally recorded.

| # | §17 P0 precondition | Status | Evidence / reason |
|---|---|---|---|
| 1 | Frozen, tagged baseline for the evidence | **PASS (frozen) / FAIL (tag)** | All evidence above is from detached `6b881a1` with an empty worktree. No tag was created because this task forbids commits/tags. |
| 2 | `demo test` passes with the Rust backend | **WAIVED (W1)** | 70 passed / 5 failed; all five are controller-only (F19, F20, F25, F26×2). No node/media failure. W1 recorded 2026-11-25. |
| 3 | One-release overlap release + rollback tag | **PASS** | `rust-cutover-0.1.0-python-rollback` created on `6b881a1` (2026-11-25) with waiver W1 recorded. |
| 4 | No parity row backed only by a Python tree | **N/A for overlap / FAIL for deletion** | Python trees remain during overlap. The §17.3 P0 4 provider list is unchanged; no drop decision was recorded. |
| 5 | No runtime/CI/build/test references to the Python trees | **N/A for overlap / FAIL for deletion** | CI/build/dev/test references remain unchanged; not re-audited here. |
| 6 | Container clean-host gate proven at this HEAD | **PASS** | `--container --full`, rootless Podman, image `sha256:2bd1b59e…`, harness 20/0/1, workspace 2,755/0/51. |
| 7 | `ozma-virtual-node` systemd supervisor live-tested | **PASS** | Real Alpine/libvirt VM discovered, real child argv, live status, clean stop/reap; see §4. |

Supporting §17.4 common-gate items re-checked here: workspace tests **PASS**;
container gate **PASS**; default Rust demo **PASS** under documented environment
isolation; unit verification **8/8 PASS**; `demo test` **FAIL**; controller
`pytest` not rerun in this task (previous frozen run: 2,813 passed / 20 skipped
/ 1 xfailed / 0 failed at `28ffd69`).

## 6. Taggability and waivers

**One-release overlap: TAGGABLE (waiver recorded).** The frozen evidence,
container gate, and live supervisor preconditions are satisfied at `6b881a1`.
The release owner recorded waiver W1 for the five controller-only `demo test`
failures and created **`rust-cutover-0.1.0-python-rollback`** on that commit.

**Python-tree deletion: NO-GO.** Unchanged: `demo test` red, no rollback tag,
Python-only providers still back parity rows with no drop decision, and
CI/build/dev/test references remain.

### Formal waivers

| ID | Scope | Status | Reason and risk |
|---|---|---|---|
| **W1** | Five `demo test` extended-suite failures: F19 `GET /api/v1/macros returns data`, F20 `automation script accepted`, F25 `AI setups used: 0`, F26 `noise_cancellation blocked on free tier`, F26 `reason mentions upgrade:`. | **RECORDED 2026-11-25** | All five assert controller-only macro/automation/Connect behaviour. The core switching/audio suite passed fully, and node HLS/snapshot/QMP input/scenario switching passed. Risk: macro/automation product behaviour remains unproven; Connect gating on the free tier is expected behaviour, not a node regression. Time-box until the next controller/macro change. |
| — | Container gate | none needed | Passed at this commit. |
| — | Supervisor QMP permissions / libvirt URI | operational accommodation, not a gate waiver | Test-only `chmod 0666` on the disposable socket and `LIBVIRT_DEFAULT_URI=qemu:///system`; the packaged unit does not yet encode either. |

No waiver is proposed for P0 4/5 in the deletion context; they remain
outstanding until the Python trees are removed or an explicit drop decision is
recorded.

## 7. Sudo, persistent changes, and cleanup

Sudo commands used (all successful, non-interactive except as noted):

```text
sudo install -m 0644 /tmp/opencode/alpine-virt-3.24.4-x86_64.iso /var/lib/libvirt/images/ozma-overlap-alpine.iso
sudo virsh define /tmp/opencode/ozma-overlap-domain.xml
sudo virsh start ozma-overlap-gate
sudo virsh destroy ozma-overlap-gate
sudo virsh undefine ozma-overlap-gate --remove-all-storage
sudo chmod 0666 /tmp/ozma-overlap-vm.qmp
sudo systemd-run --unit=ozma-overlap-virtual-node ...
sudo systemctl stop ozma-overlap-virtual-node
sudo ip netns add ozma-demo
sudo ip link add veth-ozma type veth peer name veth-ozma-ns
sudo ip link set veth-ozma-ns netns ozma-demo
sudo ip addr add 10.203.0.1/24 dev veth-ozma
sudo ip link set veth-ozma up
sudo ip netns exec ozma-demo ip addr add 10.203.0.2/24 dev veth-ozma-ns
sudo ip netns exec ozma-demo ip link set veth-ozma-ns up
sudo ip netns exec ozma-demo ip link set lo up
sudo ip netns exec ozma-demo ip route add 224.0.0.0/4 dev veth-ozma-ns
sudo ip netns exec ozma-demo ip route add default via 10.203.0.1
sudo ip netns del ozma-demo
sudo ip link del veth-ozma
sudo modprobe v4l2loopback video_nr=10,11 ...   # invoked by demo/start_vms.sh
sudo modprobe -r v4l2loopback                   # cleanup
sudo rm /var/lib/libvirt/images/ozma-overlap-alpine.iso
```

Persistent changes and their disposition:

- `/var/lib/libvirt/images/ozma-overlap-alpine.iso` — created, then removed.
- Domain `ozma-overlap-gate` — defined/started, then destroyed and undefined.
- `/tmp/ozma-overlap-vm.qmp` — removed with the domain.
- Transient unit `ozma-overlap-virtual-node` — stopped; not enabled, not
  lingering.
- Netns `ozma-demo` and veth pair — deleted.
- `v4l2loopback` — loaded by the demo's first start, then unloaded.
- No unit file installed, no service enabled, no host package installed, no
  global Python package installed.
- Repo writes: this report and the appended
  `phase4-live-cutover-report.md` section only. No product code, no
  `parity-matrix.md`, no `plan-execution-status.md`, no commit/tag.

Cleanup after evidence capture: container image and clean-host target, worktree
target, disposable venv, netns/veth, libvirt domain/ISO, demo logs/cache,
worktree, and `/tmp/opencode` scratch files. The detached worktree was removed
last; leftovers are listed in the appended live-cutover section.
