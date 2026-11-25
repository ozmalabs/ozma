# Phase 4 live cutover report: default Rust demo

Date: 2026-11-25 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Validator: Phase 4 live-cutover validation subagent
Checkout: `/home/matt/ozma/ozma`
HEAD: `1cd866d16c638ee069736d18b1a6d7a84e49b8c8`
(`fix(phase4): wire registration in main and commit controller surface helpers`)

## Verdict

The new default-Rust cutover works for the demo and installer dry-runs:

- `bash demo/run_demo.sh` **without `--rust`** started two Rust VM-profile nodes,
  and the unmodified Python controller discovered both through mDNS. Health,
  node detail, dashboard, HLS (direct and controller proxy), snapshots, QMP
  input, paste, scenario/HID switching, and graceful SIGTERM all passed.
- `bash install.sh --dry-run` plans the Rust node + helper + user unit;
  `--dry-run --legacy-python` still escapes to the AppImage path.
- Two simultaneous WebRTC peers worked when the helper dimensions matched the
  capture (1280x800).

Two real gaps were found:

1. **`demo/run_demo.sh --webrtc` cannot establish a peer.** It starts the helper
   with the 640x480 default while the QMP capture is 720x400/1280x800, so both
   demo nodes report `webrtc=failed` and return a typed 503
   `webrtc-unavailable`. The helper itself works; the launcher does not pass
   `--webrtc-width/--webrtc-height`.
2. **`demo/run_demo.sh test` leaves the stack running when a test fails.** The
   full suite ended 73 passed / 3 failed; `set -e` exited before
   `_stop_processes`. The 3 failures are controller-only Ozma Connect
   feature-gating assertions, not Rust-node failures.

Clean-host evidence on the live checkout was disrupted by a parallel workstream
editing `ozma-audio`/`ozma-node` during the run. A detached worktree at the same
HEAD passed `--host` 20/20. The rootless-container `--full` run could not start
because the locally cached image predates `libxdo-dev`; rebuilding it would
require a prohibited network `apt-get`.

This report validates the cutover defaults only. It does not claim Phase 4 is
complete and does not change any parity/status document.

## Environment

- Rust 1.95.0 / cargo 1.95.0; `target/debug/ozma-node` rebuilt at the start of
  the run (`ozma-node 0.1.0`).
- Python 3.14.10. The controller venv used for this run was created at
  `/tmp/opencode/phase4-cutover-venv` with system site packages. The available
  controller venv lacked `pynacl`, `mido`, `toml`, `strawberry-graphql`,
  `graphql-core`, `cross-web`, and `python-multipart`; they were installed from
  the **local uv cache with `--offline`**. No network install and no system
  package install was performed.
- QEMU with writable `/dev/kvm`; ffmpeg n9.1 (no VNC demuxer); avahi-daemon
  active; PipeWire/PulseAudio reachable through `pactl`; Playwright Chromium
  available for the dashboard; Python `gi` + GStreamer available for WebRTC.
- A PATH `sudo` shim (`/tmp/opencode/nosudo/sudo`) made the optional
  v4l2loopback setup fail non-fatally, as in the previous live-demo report.
- Disposable direct-QEMU Alpine 3.21.3 VMs from `demo/start_vms.sh`; no libvirt,
  no sudo, no systemd changes. Ports: controller 7380, nodes 7332/7333 (UDP)
  and 7382/7383 (HTTP), VNC 5901/5922.

The checkout was **not frozen**. At run start it already had modified routing
docs and untracked `controller/mesh_network.json` / `nodes_meta.json`. During
the run a parallel workstream wrote `ozma-audio/src/uac2.rs`,
`ozma-node/src/usb_uac2.rs`, multiseat/USB files, `Cargo.lock`, and other
sources (mtimes 11:19-11:25 local). The demo binary was built at 11:14 before
those edits and the demo scripts themselves were the current defaults. The
clean-host section below records the resulting race explicitly.

## Timestamps (UTC)

| Event | Time |
|---|---|
| Run start, HEAD + clean-start build | 2026-11-25T00:14:48Z |
| Successful default demo controller ready | 2026-11-25T00:18:16Z (11:18:16 local) |
| `demo/run_demo.sh test` start | 2026-11-25T00:21:11Z |
| `--webrtc` demo start | 2026-11-25T00:22:14Z |
| Current-checkout clean-host `--host` start | 2026-11-25T00:24:09Z |
| Detached-HEAD clean-host `--host` start | 2026-11-25T00:25:27Z |
| Clean-host `--container --full` start | 2026-11-25T00:27:09Z |
| Final cleanup check | 2026-11-25T00:27:38Z |

## PASS / FAIL / SKIP matrix

| Check | Result | Evidence |
|---|---|---|
| Build node from checkout | **PASS** | `cargo build -p ozma-node --bin ozma-node` exit 0; `ozma-node 0.1.0` |
| Default demo uses Rust without `--rust` | **PASS** | `bash demo/run_demo.sh`; log `Starting nodes (backend: rust)` and `Backend: rust (default: rust)` |
| mDNS discovery by unmodified controller | **PASS** | Controller log: `Node online: vm2… @ 192.168.40.19:7333`, `vm1… @ …:7332`; no `--register-url` |
| Node health | **PASS** | Both `/health`: 200, `ok:true`, `profile=vm`, `sink_kind=qmp`, HLS running, input available |
| Node list / detail | **PASS** | `/api/v1/nodes` and `/api/v1/nodes/<id>`: 200; both nodes with ports, capabilities, audio sink, `stream_url` |
| Dashboard cards | **PASS** | Playwright: title `Ozma — Dashboard`, `node-count=2`, 2 `.node-card[data-id]` cards (`vm2 Active`, `vm1 Ready`), 2 scenario cards, no page errors |
| Local HLS per node | **PASS** | `stream.m3u8`: 200; media sequence advanced 28 → 146; `seg_*.ts`: 200, ~100 KB |
| Controller HLS proxy | **PASS** | `/api/v1/streams/<id>/hls/stream.m3u8` 200; segment proxy 200 `video/mp2t` |
| Snapshot per node | **PASS** | `/display/snapshot` 200 `image/jpeg`, 1280x800, ~96 KB each |
| Controller snapshot proxy | **PASS** | `/api/v1/streams/<id>/snapshot` 200 `image/jpeg`, 1280x800 |
| QMP input per node | **PASS** | `/input/key` 200 `{"ok":true,"sink":"qmp"}`; `/input/type` 200, 4 events; both nodes |
| Controller paste | **PASS** | After scenario activation: `POST /api/v1/paste` 200, `chars_sent=2`; before activation it correctly returned `No active node` |
| Scenario / HID switching | **PASS** | `tests/test_e2e_switching.py --no-audio`: 24 PASS, 0 FAIL; rapid 5x switches; final state correct |
| Console switching | **SKIP (typed)** | Demo is single-head and advertises no consoles; `POST /api/v1/nodes/<id>/display/select {"index":2}` returned typed 404 `not_found` with available `[]`. Scenario switching passed. |
| Graceful node SIGTERM | **PASS** | Both nodes exited in **12-13 ms**; logs `shutdown requested … signal="SIGTERM"` and `Ozma node shutting down`; HLS ffmpeg children gone |
| Controller SIGTERM / orphan cleanup | **PASS** | Orderly shutdown log; no demo listeners, `ozma-node`, controller, QEMU, or HLS ffmpeg left |
| `demo test` switching + audio | **PASS** | Core suite 24/24, audio routing/link-swap checks all PASS |
| `demo test` full suite | **FAIL** | 73 passed, 3 failed, 0 skipped (controller-only Connect assertions; see below) |
| `demo test` cleanup on failure | **FAIL** | Stack left running after failing tests; manual `demo/run_demo.sh stop` required |
| WebRTC via `--webrtc` (two demo peers) | **FAIL** | Both nodes `webrtc=failed`; offer returned typed 503 `webrtc-unavailable`, dimension mismatch (see below) |
| WebRTC helper with matching dimensions | **PASS** | Two simultaneous probe peers: `connected`, ICE `completed`, 152 buffers / 233,472,000 bytes each, I420 1280x800@15 |
| Clean-host `--host` on live checkout | **FAIL (race)** | `cargo build` hit `E0432` while parallel work rewrote UAC2 files; see clean-host detail |
| Clean-host `--host` on detached HEAD `1cd866d` | **PASS** | 20 passed, 0 failed, 0 skipped; `ozma-node --lib` 415 passed/3 ignored; `ozma-softnode` 125 passed; synthetic `/health` PASS |
| Clean-host `--container --full` | **FAIL (environment)** | Rootless Podman works, but cached image lacks `libxdo`; build would need network `apt-get`, which was prohibited |
| `install.sh --dry-run` Rust default | **PASS** | Prints Rust node build, `ozma-node@desktop.service`, helper path; no files written |
| `install.sh --dry-run --legacy-python` | **PASS** | Prints AppImage download, `ozma-agent.service`, `uv pip install ozma-softnode` |
| Help/default text | **PASS** | Demo help says Rust default / `--python`; recorder help shows `--backend {rust,python}`; installer help documents `--dry-run`/`--legacy-python` |

## Live default demo detail

Command (no `--rust`, exercising the new default):

```bash
export PYTHON=/tmp/opencode/phase4-cutover-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node
export PATH=/tmp/opencode/nosudo:$PATH
setsid bash demo/run_demo.sh > /tmp/opencode/phase4-cutover-artifacts/demo-start3.log 2>&1 &
```

Controller discovery, with no registration URL:

```text
11:18:16 INFO ozma.discovery: mDNS browser started for _ozma._udp.local.
11:18:16 INFO ozma.discovery: Node online: vm2._ozma._udp.local. @ 192.168.40.19:7333 role=compute hw=x86_64
11:18:16 INFO ozma.discovery: Node online: vm1._ozma._udp.local. @ 192.168.40.19:7332 role=compute hw=x86_64
11:18:17 INFO ozma.stream: Remote stream registered for vm2... → http://192.168.40.19:7383/stream/stream.m3u8
11:18:17 INFO ozma.stream: Remote stream registered for vm1... → http://192.168.40.19:7382/stream/stream.m3u8
```

`/api/v1/nodes` reported both nodes with `api_port` 7382/7383,
`capabilities=[hid,video,audio,power]`, `audio_type=pipewire`,
`audio_sink=ozma-vm1|ozma-vm2`, and `stream_url`. `vm1` also advertised
`vm_guest_ip=10.0.0.43`; the controller preferred the host A record for the
stream URL. Both nodes stayed discovered for the full run.

HLS media sequence advanced from 28 to 146 between samples, and controller
segment proxying returned `video/mp2t`. Snapshots decoded as 1280x800 JPEG on
both the direct node and the controller proxy.

Dashboard verification used Playwright Chromium against
`http://localhost:7380/dashboard.html` (the root `/` is the 3D desk UI):

```text
TITLE Ozma — Dashboard
NODE_COUNT 2
NODE_CARDS 2
CARD: vm2 | Active | 🔑 Token
CARD: vm1 | Ready | Switch | 🔑 Token
SCENARIO_CARDS 2
PAGE_ERRORS []
```

The page has a third element with class `node-card` for the hotkeys/controls
panel; counting `[data-id]` correctly gives the two nodes. This is a UI class
reuse, not a missing-node regression.

## Shutdown detail

`SIGTERM` was sent to both node pidfiles while QEMU and the controller kept
running:

```text
ozma-sn1 exited in 13ms
ozma-sn2 exited in 12ms
shutdown requested; stopping node tasks signal="SIGTERM"
Ozma node shutting down
```

HLS ffmpeg children disappeared. The controller stayed healthy (`/api/v1/status`
200) until `demo/run_demo.sh stop`, which logged an orderly shutdown. The demo
pidfiles, QMP sockets, listeners, and processes were all gone afterwards.

## `demo test` detail

`bash demo/run_demo.sh test` started the default Rust stack and ran
`tests/test_e2e_switching.py` (with audio) plus `tests/test_e2e_features.py`.
Switching and audio checks passed. The extended suite ended:

```text
73 passed, 3 failed, 0 skipped
```

The three failures are controller-only:

- `[F25]` `usage` from `GET /api/v1/connect/status` lacks `ai_setups`.
  Actual response had `usage={"nodes":2,"controllers":1}` and
  `limits={"controllers":-1,"nodes":-1}`.
- `[F26]` `GET /api/v1/connect/check/noise_cancellation` returned
  `{"allowed":true,"feature":"noise_cancellation","tier":"free"}` instead of a
  blocked response with a Pro upgrade reason.

These endpoints do not depend on the node backend and are not caused by the
Rust cutover. They are recorded as controller test/config drift.

The test branch also exposed a cleanup defect:

```text
bash demo/run_demo.sh test   # exits 1
pgrep: ozma-node/controller/QEMU still running
bash demo/run_demo.sh stop   # required manual cleanup
```

`run_tests` returning non-zero under `set -euo pipefail` exits before
`_stop_processes` runs. This may predate the cutover, but it was observed here
and is a real demo-harness defect.

## WebRTC detail

`bash demo/run_demo.sh --webrtc` (still no `--rust`) used the Rust default and
passed the helper flags. Both nodes logged `WebRTC helper started`, then failed:

```text
webrtc=failed
webrtc_error=frame dimensions 720x400 do not match the WebRTC helper's 640x480
```

A valid non-empty offer returned HTTP 503:

```json
{"ok":false,"code":"webrtc-unavailable",
 "error":"frame dimensions 720x400 do not match the WebRTC helper's 640x480",
 "remediation":"Check the node log and /api/v1/connection.webrtc_error; ..."}
```

The same typed 503 came from the controller `/api/v1/streams/<id>/webrtc`
proxy. The demo launcher passes only `--webrtc-helper` and
`--webrtc-helper-script`; it does not pass `--webrtc-width/--webrtc-height`, so
the helper keeps its 640x480 default while QMP frames are larger.

To separate launcher wiring from helper capability, two ephemeral probe nodes
were started against the same QMP sockets with explicit matching dimensions
(1280x800@15), then two `scripts/gate2/webrtc-client.py` peers ran
concurrently:

```text
wprobe1: ok=true, connection_state=connected, ice=completed,
         152 video buffers, 233472000 bytes,
         video/x-raw I420 1280x800@15
wprobe2: ok=true, connection_state=connected, ice=completed,
         152 video buffers, 233472000 bytes,
         video/x-raw I420 1280x800@15
```

Both clients exited 0. No audio transceiver was offered. The helper path is
functional; the `--webrtc` demo wiring is not.

## Clean-host detail

### Current live checkout: FAIL (parallel-edit race)

`scripts/clean-host/run.sh --host` started at 00:24:09Z. A parallel workstream
was writing UAC2 files at the same time (mtimes 11:19-11:25 local). The build
read an intermediate state:

```text
error[E0432]: unresolved imports `ozma_audio::Uac2PcmFormat`, `ozma_audio::Uac2SampleSize`
  --> ozma-node/src/usb_uac2.rs:37:51
FAIL  cargo build -p ozma-node -p ozma-softnode --bins
```

A follow-up `cargo check -p ozma-node` at 00:25:07Z still failed with
`E0433: cannot find module or crate usb_uac2` because `main.rs` referenced the
new module while the tree was mid-write. This is a moving-checkout artifact,
not a stable HEAD failure.

### Detached HEAD worktree: PASS

To isolate the cutover commit, a detached worktree at the same HEAD was created:

```bash
git worktree add --detach /home/matt/ozma/ozma-cutover-head-1cd866d 1cd866d
cd /home/matt/ozma/ozma-cutover-head-1cd866d
CARGO_NET_OFFLINE=true \
OZMA_CLEAN_HOST_TARGET=/home/matt/ozma/ozma-cutover-head-target \
OZMA_CLEAN_HOST_LOG_DIR=/tmp/opencode/phase4-cutover-artifacts/clean-host-clean \
    bash scripts/clean-host/run.sh --host
```

Result:

```text
PASS  cargo build -p ozma-node -p ozma-softnode --bins
PASS  cargo test -p ozma-node --lib        (415 passed, 3 ignored)
PASS  cargo test -p ozma-softnode          (125 passed)
PASS  ozma-node desktop profile answered /health on port 39100
20 passed, 0 failed, 0 skipped
RESULT: PASS
```

### Rootless container `--full`: FAIL (stale image / environment)

Podman is rootless and functional, but `localhost/ozma-clean-host:1.95` was not
tagged. A locally built untagged image was reused (tagged temporarily, no
network build). The container dependency check then failed:

```text
FAIL  libxdo is missing (enigo link dependency for ozma-hid tests)
Summary: 10 passed, 1 failed, 8 skipped
FAIL  native build dependencies
RESULT: FAIL
```

The cached image predates `libxdo-dev` in the current Containerfile. Building a
fresh image runs `apt-get update/install`, which this task prohibited. The
container run is therefore **not** a product failure; it is an environment
blocker. The temporary image tag was removed afterwards.

## Installer dry-run detail

Default path (`OZMA_DIR` set to a non-existent temp path):

```text
DRY   bash .../scripts/install-rust.sh --build --prefix /home/matt/.local \
          --bin-dir /home/matt/.local/bin --units-dir /home/matt/.config/systemd/user
DRY   systemctl --user enable --now ozma-node@desktop.service
DRY   webrtc helper: /home/matt/.local/share/ozma/webrtc/gst_webrtc_worker.py
```

Legacy path:

```text
DRY   curl .../ozma-agent-linux-x86_64.AppImage -o /home/matt/.local/bin/ozma-agent
DRY   write .../ozma-agent.service
DRY   systemctl --user enable --now ozma-agent
```

Both exited 0. The temp `OZMA_DIR` was not created. `demo/run_demo.sh --help`
says `Rust ozma-node (default)` and documents `--python`; recorder help shows
`--backend {rust,python}`; installer help documents `--dry-run` and
`--legacy-python`.

## Regressions and findings

1. **`--webrtc` demo is broken by a dimension mismatch.** The new flag wires
   the helper but not the capture dimensions. Both demo nodes return 503
   `webrtc-unavailable`; two-peer verification via the demo failed. The helper
   works when explicitly given 1280x800. Suggested fix (not applied): pass
   `--webrtc-width/--webrtc-height` from the capture or derive them from the
   first frame.
2. **`demo/run_demo.sh test` does not clean up after a failing suite.** It left
   nodes, controller, and QEMU running. The manual stop was clean.
3. **Controller-only Connect assertions fail** (`ai_setups` missing;
   `noise_cancellation` allowed on free tier). Not Rust-node related.
4. **The live checkout was mid-edit**; clean-host first failed on UAC2 compile
   imports. The same HEAD in a detached worktree passed. This is an environment
   caveat for any evidence taken from the moving checkout.
5. **The cached clean-host container image is stale** (no `libxdo`), so
   `--container --full` could not run without a network image build.
6. **No mDNS, HLS, snapshot, QMP-input, paste, switching, or shutdown
   regression was observed.**

## Artifacts

`/tmp/opencode/phase4-cutover-artifacts/` contains:

- `run-timestamps.txt`, `git-status-before.txt`, `build.log`
- `demo-start.log`, `demo-start2.log`, `demo-start3.log`, `demo-test.log`,
  `demo-webrtc-start.log`, stop logs
- `demo-logs/` (controller, node-vm1, node-vm2, VM serial logs)
- `controller-status.json`, `controller-nodes.json`, `node-*.json`,
  `health-*.json`, `input-*.json`, `ctrl-paste*.json`, `display-select-*.json`
- `playlist-*.m3u8`, `ctrl-playlist-*.m3u8`, `seg-*.ts`, `ctrl-seg.ts`,
  `snapshot-*.jpg`, `ctrl-snapshot-*.jpg`
- `e2e-switching.log`, `dashboard-cutover.png`, `dashboard-playwright.log`
- `webrtc-*.json`, `webrtc-client-*.json`, `webrtc-client-*.log`,
  `wprobe1.log`, `wprobe2.log`
- `clean-host/host-run.log`, `clean-host-clean/host-run.log`,
  `clean-host-clean/run.log`, and the console logs
- `install-dry-run.log`, `install-dry-run-legacy.log`, `install-help.log`,
  `demo-help.log`, `record-help.log`
- `connect-status.json`, `connect-noise.json`

## Cleanup status

- No `ozma-node`, controller, QEMU, HLS ffmpeg, or `gst_webrtc_worker`
  processes remain.
- No demo ports (7332/7333, 7380-7385, 5901/5922, probe 17331/17333/17382/17383)
  are listening.
- Demo pidfiles, `/tmp/ozma-vm*.qmp`, probe sockets, and `/dev/shm/wprobe*`
  files are removed.
- The two PipeWire null sinks created by the demo (`ozma-vm1`, `ozma-vm2`) were
  unloaded.
- Controller state created by this run (`moonlight_data/`, `msp_data/`,
  `scan_data/`, `static/captures/`, `wg_keys.json`, `wg_state.json`) was
  removed. Pre-existing untracked `controller/mesh_network.json` and
  `nodes_meta.json` were left in place; the controller touched
  `nodes_meta.json` during discovery.
- The detached HEAD worktree, its target directory, and the temporary controller
  venv were removed. The temporary container image tag was removed.
- No source changes were made to tracked product code; the only file added by
  this task is this report.

## Remaining Phase 4 blockers (not re-evaluated)

The controller is still Python, privileged install assets and a Python-free
fresh-host install remain unproven, WebRTC in the demo needs the dimension fix,
and the Python node trees still exist for the one-release overlap. This run does
not claim Phase 4 completion.

## Follow-up fixes (2026-11-25, after this run)

Two of the regressions above were fixed immediately; the live WebRTC path needs
a re-run to confirm them end to end.

1. **WebRTC geometry.** `ozma-node` gained `--webrtc-fit-capture`
   (`OZMA_WEBRTC_FIT_CAPTURE`). When set, the single active helper uses the
   capture source's frame size instead of requiring an exact
   `--webrtc-width/--webrtc-height` match; the resolved geometry is kept for
   later console-switch restarts. `demo/run_demo.sh --webrtc` passes it.
   The first re-validation showed QMP reports `0x0` until its first
   screendump, so the initial implementation still spawned at 640x480. The
   fit path now probes one bounded frame (3 s) before spawning when the
   discovery format is unknown; a failed/timed-out probe keeps the configured
   size and the helper's own dimension check still fails honestly. A second
   re-validation then showed the guest modesetting from 720x400 to 1280x800
   after startup, so fit mode now also nearest-neighbour scales later frames to
   the helper size instead of killing the feeder (without the flag a mismatch
   stays a typed error). A live two-peer re-run against the current binary is
   required to confirm the full path.
2. **`demo test` cleanup.** The test subcommand now records a non-zero result
   without tripping `set -e` and always calls `_stop_processes`, including when
   the stack fails to start, so a failing suite no longer leaves nodes, QEMU,
   ffmpeg, and the controller running.

The controller-only Connect test drift and the stale container image
(`libxdo`) are unchanged and unrelated to the Rust node.

## WebRTC re-validation (2026-11-25, after the geometry fix)

Date: 2026-11-25 11:42-11:49 local (00:42-00:49 UTC).
Checkout: `/home/matt/ozma/ozma`, HEAD
`2f07a59a2cf87358e05fda8bd70b2ded80fba043`
(`feat(phase4): UAC2 bridge wiring, USB completeness, seat input isolation`),
which contains the `--webrtc-fit-capture` follow-up.
Node binary: `target/debug/ozma-node` 0.1.0, built 11:39 local, before this run.

The previous controller venv and detached worktree from the cutover run had
already been removed. A fresh controller venv was created with the system
Python 3.14 and system site packages, then the missing pure-Python/ABI3
packages (`pynacl`, `mido`, `toml`, `strawberry-graphql`, `graphql-core`,
`cross-web`, `python-multipart`) were installed **offline from the local uv
cache with `--no-deps`**. No network install, no system package, no sudo.
GStreamer `gi` came from the system Python, as before.

The checkout was **not frozen**: while this validation ran, a parallel
workstream modified `Cargo.lock`, `controller/api.py`, `ozma-mesh`,
`ozma-node/src/{main,controller_compat,lib}.rs`, `ozma-usb`, and added new
untracked source files (mtimes 11:44-11:48 local). The node binary used here
predates those edits. No source files were changed by this validation.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| `demo/run_demo.sh --webrtc`: both nodes `/api/v1/connection.webrtc` = `running` | **FAIL** | Both report `"webrtc":"failed"`; helper starts at 640x480, not the capture size |
| Fitted geometry reported by the node | **FAIL** | Log still says `width=640 height=480`; no `fitting WebRTC helper` line |
| Typed WebRTC offer on the demo nodes | **FAIL (expected for this state)** | 503 `webrtc-unavailable`, `frame dimensions 720x400 do not match the WebRTC helper's 640x480` |
| Controller WebRTC proxy reaches the node | **PASS** | Same typed 503 body through `/api/v1/streams/<id>/webrtc` (not a proxy 404/502) |
| Two real peers with matching geometry (demo nodes) | **PASS** | 2 concurrent GStreamer peers, `connected` / ICE `completed`, I420 1280x800@30 |
| Controller proxy peer for one node | **PASS** | Third peer through `/api/v1/streams/vm1.../webrtc`: `connected`, ICE `completed` |
| Console switching | **SKIP (typed)** | Single-head demo advertises `display_outputs=[]`; `display/select index=2` returns typed 404 `not_found` |
| QMP input per node | **PASS** | `/input/key` `{"ok":true,"sink":"qmp"}` and `/input/type` 4 events on both 7382/7383 |
| Snapshot / HLS / controller proxies | **PASS** | 1280x800 JPEG and H.264 1280x800 segments, direct and proxied |
| Helper PIDs / sockets per node | **PASS** | Two distinct `gst_webrtc_worker.py` PIDs, one socket and one 4,096,000-byte shm per node |
| `demo/run_demo.sh test` cleanup on a failing suite | **PASS** | 73 passed / 3 failed, then nodes, controller, and both QEMU exited; no orphans |

### Run A: literal `--webrtc` still fails on QMP geometry

Command (same local QEMU/controller environment as the prior report):

```bash
export PYTHON=/tmp/opencode/webrtc-reval-ctrl-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node
export PATH=/tmp/opencode/nosudo:$PATH
setsid bash demo/run_demo.sh --webrtc \
  > /tmp/opencode/webrtc-reval-artifacts/demo-webrtc-start.log 2>&1 &
```

Both node processes did receive the new flag (verified from
`/proc/<pid>/cmdline`), and the demo printed `WebRTC: helper enabled`. The
node log nevertheless started the helper at the default size:

```text
00:42:58.672Z INFO ozma_node: WebRTC helper started
  pid=Some(1646507) socket=/tmp/ozma-webrtc-vm1-1646471.sock
  shm=/dev/shm/ozma-webrtc-fb-1646471 width=640 height=480 fps=30
```

No `fitting WebRTC helper to the capture frame size` line appeared. The
connection endpoint and the offer were both refused:

```text
GET /api/v1/connection   → "webrtc":"failed"
                           "webrtc_error":"frame dimensions 720x400 do not match
                           the WebRTC helper's 640x480"

POST /webrtc/offer       → HTTP 503
{"ok":false,"code":"webrtc-unavailable",
 "error":"frame dimensions 720x400 do not match the WebRTC helper's 640x480", ...}
```

The controller proxy was healthy but returned the node's typed error, not a
proxy error:

```text
POST /api/v1/streams/vm1._ozma._udp.local./webrtc → HTTP 503
same {"code":"webrtc-unavailable", "error":"frame dimensions 720x400 ..."}
```

**Why the fix does not take effect for QMP.** `main.rs` applies
`--webrtc-fit-capture` only after `open_first_capture_source(...)` and only
when `source.format()` already has non-zero dimensions:
`ozma-node/src/main.rs:1670-1691`. But `QmpDisplaySource` deliberately declares
`FrameFormat::rgb24(0, 0)` until the first screendump arrives
(`ozma-capture/src/qmp_display.rs:337-339`). The fit therefore sees `0x0`,
leaves `640x480` in place, and starts the helper before any frame exists. The
first QMP frame during Alpine boot was `720x400`; the head later settles at
`1280x800` (snapshots and HLS segments below), so the helper is permanently
`failed` for the life of that node. This is a real remaining defect in the
follow-up fix for runtime-geometry providers, not a launcher omission.

### Run B: peers work when the demo node geometry matches

To separate helper/peer capability from the fit bug, the two nodes and the
controller were stopped while the QEMU VMs stayed running at 1280x800, then the
same demo was started without restarting the VMs and with explicit matching
dimensions. `--webrtc-fit-capture` was still passed by the launcher but is
inert here because explicit values were supplied:

```bash
export OZMA_WEBRTC_WIDTH=1280 OZMA_WEBRTC_HEIGHT=800
setsid bash demo/run_demo.sh --webrtc --no-vms \
  > /tmp/opencode/webrtc-reval-artifacts/demo-webrtc-1280x800.log 2>&1 &
```

Both nodes reported `"webrtc":"running"` with `webrtc_error=null`. Helper
evidence:

```text
00:45:41.676Z INFO ozma_node: WebRTC helper started
  pid=Some(1654703) socket=/tmp/ozma-webrtc-vm1-1654668.sock
  shm=/dev/shm/ozma-webrtc-fb-1654668 width=1280 height=800 fps=30

00:45:41.676Z INFO ozma_node: WebRTC helper started
  pid=Some(1654704) socket=/tmp/ozma-webrtc-vm2-1654671.sock
  shm=/dev/shm/ozma-webrtc-fb-1654671 width=1280 height=800 fps=30
```

`ps` confirmed two separate helpers, each parented by its own node
(1654703→1654668, 1654704→1654671), with one Unix socket and one 4,096,000-byte
shm file per node (`1280*800*4`).

Two `scripts/gate2/webrtc-client.py` peers started concurrently, one per node,
each with `--duration 8`:

```text
vm1 (direct :7382): ok=true connection_state=connected ice=completed
  238 video buffers, 365,568,000 bytes
  video/x-raw I420 1280x800 progressive 30/1
vm2 (direct :7383): ok=true connection_state=connected ice=completed
  236 video buffers, 362,496,000 bytes
  video/x-raw I420 1280x800 progressive 30/1
```

Both client processes exited 0. A third peer used the controller proxy as its
offer URL:

```text
--offer-url http://localhost:7380/api/v1/streams/vm1._ozma._udp.local./webrtc
ok=true connection_state=connected ice=completed
180 video buffers, 276,480,000 bytes, I420 1280x800@30, exit 0
```

This re-confirms the helper and the controller proxy path. It does **not**
rehabilitate Run A: the default `--webrtc` invocation still needs geometry to
be supplied out of band.

### Console, snapshot, HLS, and input

On the same Run A nodes (before restart) and again on Run B:

- `GET /health` 200 on 7382/7383, `ok:true`, 0 sessions.
- `GET /display/snapshot` 200 `image/jpeg`, 96,470 / 96,488 bytes, decoded
  **1280x800**. Controller `/api/v1/streams/<id>/snapshot` matched.
- Local HLS playlists advanced (media sequences 118/119 at sample time), first
  segment 200 `video/vnd.dlna.mpeg-tts` ~100 KB; `ffprobe` reported
  `h264,1280,800` on both. Controller HLS proxy returned 200
  `application/vnd.apple.mpegurl` plus `video/mp2t` segment, also H.264
  1280x800.
- `/input/key {"keycode":30,...}` → `{"ok":true,"sink":"qmp"}` and
  `/input/type` → `{"ok":true,"events":4}` on both nodes.
- `POST /display/select {"index":2}` → typed 404 `not_found` with
  `available:[]`; same single-head limitation as the cutover run, not a
  regression.

No dimension regression was observed in capture, snapshots, or HLS: all were
1280x800. The only wrong geometry was the helper's 640x480 default in Run A.

### `demo test` cleanup on a failing suite

Command (no `--webrtc`):

```bash
export PYTHON=/tmp/opencode/webrtc-reval-ctrl-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node
export PATH=/tmp/opencode/nosudo:$PATH
setsid bash demo/run_demo.sh test \
  > /tmp/opencode/webrtc-reval-artifacts/demo-test.log 2>&1 &
```

Started 00:47:20Z, finished 00:47:57Z. The suite still has the two
controller-only Connect drifts from the cutover run:

```text
[F25] Ozma Connect status      FAIL  AI setups used: 0
[F26] Ozma Connect gating      FAIL  noise_cancellation blocked on free tier
                               FAIL  reason mentions upgrade:
73 passed, 3 failed, 0 skipped
SOME TESTS FAILED
```

Because the suite failed, this directly exercises the cleanup fix. The harness
stopped the whole stack without manual intervention:

```text
Stopping Ozma demo...
  killed PID 1661233 (ozma-ctrl)
  killed PID 1661228 (ozma-sn1)
  killed PID 1661231 (ozma-sn2)
Stopping VMs...
  killed PID 1661172
  killed PID 1661174
Done.
```

Post-run checks found no `ozma-node`, controller, QEMU, `gst_webrtc_worker.py`,
or ffmpeg processes; no listeners on 7332/7333, 7380-7385, 5901/5922; and no
demo pidfiles or `/tmp/ozma-vm*.qmp` sockets. The cleanup fix is **PASS**.
(The 3 test failures are unchanged controller test drift, unrelated to the
Rust node and to WebRTC.)

### Remaining WebRTC gaps after this re-validation

1. **`--webrtc-fit-capture` is a no-op for QMP.** The helper is fitted before
   the first frame exists, and `QmpDisplaySource.format()` is `0x0` until then.
   The default demo therefore still reports `webrtc=failed` with a typed 503.
   A real fix would defer `WebRtcHelper::start` until the first frame, or
   re-fit/restart the helper from the first observed frame. Not applied here.
2. **Geometry is not exposed on the status API.** `/api/v1/connection` reports
   only `"webrtc":"running"` plus `webrtc_error`; width/height/fps are visible
   only in the node log and in peer SDP caps. If consumers need the fitted
   geometry, the status block needs explicit fields.
3. **WebRTC audio remains unconfigured** (`webrtc_audio_configured=false`,
   `webrtc_audio_source=null`); the demo does not pass an audio source, and the
   peers negotiated video only. This matches the documented scope, not a
   regression.
4. **No native WebRTC.** The GStreamer/Python helper is still required, as the
   profile warning states.
5. **The checkout moved during validation.** The binary predates the parallel
   edits; the fit finding is against the committed 11:32 `main.rs` and the
   11:39 binary. A frozen-tree re-run would remove this caveat but is not
   expected to change the QMP result, which follows directly from
   `qmp_display.rs`.

### Artifacts and cleanup

Artifacts are in `/tmp/opencode/webrtc-reval-artifacts/`: start/stop logs,
`connection-*.json` (failed and running), `webrtc-offer-*.txt`,
`ctrl-webrtc-*.txt`, peer JSON/logs, snapshot JPEGs, HLS playlists/segments,
`hls-check.txt`, `controller-proxy-check.txt`, `input-console-check.txt`,
`helper-procs*.txt`, `demo-test.log`, and node/controller log copies.

Cleaned up after the run:

- All nodes, controller, QEMU, helper, HLS ffmpeg, and peer processes are gone;
  no demo ports are listening and no pidfiles/QMP sockets remain.
- `demo/logs/`, controller state created by this run (`moonlight_data/`,
  `msp_data/`, `scan_data/`, `static/captures/`, `wg_keys.json`,
  `wg_state.json`, `connect_cache.json`), `/tmp/ozma-qmp-*.ppm`, and the
  temporary controller venv were removed.
- Two stale WebRTC leftovers from an older Nov 24 run
  (`/tmp/ozma-webrtc-ozma-gate2-matrix-216313.sock`,
  `/dev/shm/ozma-webrtc-fb-216313`) had no owning process and were removed.
- Pre-existing untracked `controller/mesh_network.json` and `nodes_meta.json`
  were left in place, as in the prior report.
- No product source was edited and nothing was committed. The parallel
  workstream's modified/untracked source files are untouched.

## WebRTC fit recheck (2026-11-25)

Date: 2026-11-25 12:02-12:10 local (01:02-01:10 UTC).
Checkout: `/home/matt/ozma/ozma`, HEAD
`16f77911a09e627ae9aa2fe3f5100b1858d0088d`
(`feat(phase4): FunctionFS SCSI media loop and mesh Connect service`), which
contains the bounded one-frame probe in `ozma-node/src/main.rs:1711-1749`.
Node binary: `target/debug/ozma-node` 0.1.0, rebuilt at 12:05:31 local with
`CARGO_NET_OFFLINE=true cargo build -p ozma-node --bin ozma-node` (exit 0).

Environment was the same local demo setup as the previous re-validation:
direct-QEMU Alpine 3.21.3 VMs from `demo/start_vms.sh`, QMP sockets
`/tmp/ozma-vm{1,2}.qmp`, controller 7380, nodes 7382/7383, VNC 5901/5922. A
fresh controller venv at `/tmp/opencode/webrtc-fit-recheck-venv` was created
with system site packages and the missing pure-Python/ABI3 packages installed
**offline from the local uv cache** (`uv pip install --offline --no-deps`).
No network install, no system package, no sudo. The checkout moved during the
run (`ozma-multiseat` edits by a parallel workstream); no source was edited by
this validation.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| Build current `ozma-node` | **PASS** | `cargo build` exit 0; `ozma-node 0.1.0` |
| Literal `demo/run_demo.sh --webrtc` emits the fit line on both nodes | **PASS** | `fitting WebRTC helper to the capture frame size configured=640x480 capture=720x400` on vm1 and vm2 |
| Helper argv uses the probed head size, not 640x480 | **PASS (but wrong head)** | helper argv `--width 720 --height 400`; the guest later settles at 1280x800 |
| `/api/v1/connection.webrtc` = `running` on both demo nodes | **FAIL** | both `"webrtc":"failed"` |
| `webrtc_error` absent on both demo nodes | **FAIL** | `frame dimensions 1280x800 do not match the WebRTC helper's 720x400` |
| Two real peers on the literal `--webrtc` demo | **FAIL** | both peers HTTP 503 `webrtc-unavailable`, 0 buffers |
| Controller `/api/v1/streams/<id>/webrtc` proxy on literal demo | **FAIL (proxy healthy)** | same typed 503 body, not a proxy 404/502 |
| Snapshot / HLS / input on literal demo | **PASS** | JPEG 1280x800, H.264 1280x800 segments, QMP key/type ok |
| Fit when the first observed frame is the settled head | **PASS** | Run C: `capture=1280x800`, helper `1280x800`, both `webrtc=running` |
| Two real peers with the fitted 1280x800 helper (Run C) | **PASS** | 2 concurrent GStreamer peers, `connected` / ICE `completed`, I420 1280x800@30 |
| Controller proxy peer with the fitted helper (Run C) | **PASS** | third peer via `/api/v1/streams/vm1.../webrtc`, `connected` / ICE `completed` |
| Console switching | **SKIP (typed)** | single-head demo, `display_outputs=[]`, `display/select index=2` → typed 404 `not_found` |
| `demo/run_demo.sh test` runs and cleans up | **PASS** | 73 passed / 3 failed (unchanged Connect drift), stack and QEMU stopped automatically |

### Run A: literal `--webrtc` still ends at the wrong geometry

Command:

```bash
export PYTHON=/tmp/opencode/webrtc-fit-recheck-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node
export PATH=/tmp/opencode/nosudo:$PATH
setsid bash demo/run_demo.sh --webrtc \
  > /tmp/opencode/webrtc-fit-recheck-artifacts/demo-webrtc-start.log 2>&1 &
```

The new probe does run. Both nodes logged (01:05:42Z / 12:05:42 local):

```text
INFO ozma_node: fitting WebRTC helper to the capture frame size
  configured=640x480 capture=720x400
INFO ozma_node: WebRTC helper started
  width=720 height=400 fps=30
```

`ps` confirmed the helper argv matched: `--width 720 --height 400`. The
screendump at node start is the early VGA mode (720x400); once the Alpine
guest switches the framebuffer, capture is 1280x800. The helper is not re-fitted
when the capture geometry changes, so it stays at 720x400 for the node's life:

```text
GET /api/v1/connection (7382/7383)
  "webrtc":"failed"
  "webrtc_error":"frame dimensions 1280x800 do not match the WebRTC helper's 720x400"
```

A genuine GStreamer peer per node (`scripts/gate2/webrtc-client.py`,
`--duration 8`) failed with exit 1 and zero media:

```text
vm1: HTTP 503 from http://127.0.0.1:7382/webrtc/offer:
     {"code":"webrtc-unavailable","error":"frame dimensions 1280x800 do not
      match the WebRTC helper's 720x400", ...}
     connection_state=unknown ice_connection_state=unknown
     0 video buffers
vm2: same typed 503 from :7383, connection_state=unknown, 0 buffers
```

The controller proxy is healthy and returns the node's typed error unchanged:

```text
POST http://localhost:7380/api/v1/streams/vm1._ozma._udp.local./webrtc
  → HTTP 503 {"code":"webrtc-unavailable",
              "error":"frame dimensions 1280x800 do not match the WebRTC
                       helper's 720x400", ...}
```

So the specific previous bug ("fit runs before the first screendump and stays
640x480") is fixed, but the default demo still fails because the first frame is
a transient boot mode, not the final head size.

### Run B: explicit 1280x800 (isolation control)

Nodes and controller were stopped while the same QEMU VMs kept running.
The demo was restarted with `--no-vms` and explicit matching dimensions:

```bash
export OZMA_WEBRTC_WIDTH=1280 OZMA_WEBRTC_HEIGHT=800
setsid bash demo/run_demo.sh --webrtc --no-vms \
  > /tmp/opencode/webrtc-fit-recheck-artifacts/demo-webrtc-1280x800.log 2>&1 &
```

Both nodes reported `"webrtc":"running"`, `webrtc_error=null`, and helpers at
`width=1280 height=800`. Two concurrent peers and one proxy peer all connected:

```text
vm1 direct:  connected, ICE completed, 237 buffers, 364,032,000 bytes,
             I420 1280x800 progressive 30/1
vm2 direct:  connected, ICE completed, 238 buffers, 365,568,000 bytes,
             I420 1280x800 progressive 30/1
vm1 proxy:   connected, ICE completed, 240 buffers, 368,640,000 bytes,
             I420 1280x800 progressive 30/1
```

### Run C: fitted helper when the first observed frame is the head

To test the fit path without an explicit size override, the VMs were started,
allowed to settle for 20 s, and the demo was then started with `--no-vms` and
no `OZMA_WEBRTC_WIDTH/HEIGHT`:

```bash
bash demo/start_vms.sh
sleep 20
setsid bash demo/run_demo.sh --webrtc --no-vms \
  > /tmp/opencode/webrtc-fit-recheck-artifacts/demo-webrtc-runC-settled.log 2>&1 &
```

Both nodes fitted to the real head and started running helpers (01:09:05Z):

```text
INFO ozma_node: fitting WebRTC helper to the capture frame size
  configured=640x480 capture=1280x800
INFO ozma_node: WebRTC helper started width=1280 height=800 fps=30
```

Helpers: `--width 1280 --height 800`, distinct PIDs parented by each node, one
Unix socket and one 4,096,000-byte shm each. Peers:

```text
vm1 direct:  connected, ICE completed, 237 buffers, 364,032,000 bytes,
             I420 1280x800 progressive 30/1, exit 0
vm2 direct:  connected, ICE completed, 241 buffers, 370,176,000 bytes,
             I420 1280x800 progressive 30/1, exit 0
vm1 proxy:   connected, ICE completed, 240 buffers, 368,640,000 bytes,
             I420 1280x800 progressive 30/1, exit 0
```

This isolates the remaining defect: the fit mechanism itself works; the
default demo simply probes during the guest's boot-time mode change.

### Snapshot, HLS, input, console

On Run A, Run B, and Run C nodes:

- `GET /display/snapshot` 200 `image/jpeg`, decoded **1280x800**
  (~96 KB); controller `/api/v1/streams/<id>/snapshot` matched.
- Local HLS playlist 200; segments 200, `ffprobe` reported `h264,1280,800`.
  Controller HLS proxy returned 200 `application/vnd.apple.mpegurl`; proxied
  snapshot decoded 1280x800.
- `/input/key {"keycode":30,...}` → `{"ok":true,"sink":"qmp"}` and
  `/input/type {"text":"ab"}` → `{"ok":true,"events":4}` on both nodes.
- `POST /display/select {"index":2}` → typed 404 `not_found` with
  `available:[]` (single-head demo; not a regression).

No capture/snapshot/HLS geometry regression. The only wrong geometry is the
helper's 720x400 in the literal `--webrtc` start.

### `demo test` cleanup

Command (no `--webrtc`), same venv/binary environment:

```bash
setsid bash demo/run_demo.sh test \
  > /tmp/opencode/webrtc-fit-recheck-artifacts/demo-test.log 2>&1 &
```

Result: exit 1 from the unchanged controller-only Connect assertions:

```text
FAIL  AI setups used: 0
FAIL  noise_cancellation blocked on free tier
FAIL  reason mentions upgrade:
73 passed, 3 failed, 0 skipped
SOME TESTS FAILED
```

The cleanup fix worked without manual intervention:

```text
Stopping Ozma demo...
  killed PID 1771546 (ozma-ctrl)
  killed PID 1771541 (ozma-sn1)
  killed PID 1771544 (ozma-sn2)
Stopping VMs...
  killed PID 1771518
  killed PID 1771520
Done.
```

Post-run checks found no `ozma-node`, controller, QEMU, helper, or HLS ffmpeg
processes; no listeners on 7332/7333, 7380, 7382/7383, 5901/5922; and no demo
pidfiles or QMP sockets. Cleanup is **PASS**.

### Remaining WebRTC gaps after the fit recheck

1. **The probe captures a transient boot frame, not a stable head.** The QMP
   head is 720x400 during early guest boot and 1280x800 once Alpine switches
   the framebuffer. Probing one frame therefore fits 720x400 and the helper is
   never re-fitted when the capture grows, so the literal `--webrtc` demo still
   reports `failed` with a typed 503. A complete fix should wait for a stable
   size, re-fit/restart on a capture-format change, or defer helper start until
   after boot; alternatively the demo should start nodes after the VM settles.
   Not applied here.
2. **Geometry is still not exposed on the status API.**
   `/api/v1/connection` reports only `webrtc: running|failed` plus
   `webrtc_error`; width/height/fps are visible only in the node log, helper
   argv, and peer caps.
3. **WebRTC audio remains unconfigured** (`webrtc_audio_configured=false`,
   `webrtc_audio_source=null`); peers negotiated video only. This matches the
   documented scope, not a regression.
4. **No native WebRTC.** The external GStreamer/Python helper is still
   required.
5. **The checkout moved during validation.** The binary was built from
   `16f7791` at 12:05:31; a parallel workstream edited `ozma-multiseat` while
   the run proceeded. This does not touch the WebRTC/capture path.
6. **Controller test drift is unchanged** (the 3 Connect failures above), and
   is unrelated to the Rust node or WebRTC.

### Artifacts and cleanup

Artifacts were written to `/tmp/opencode/webrtc-fit-recheck-artifacts/` (start
and stop logs, connection JSON, offer/peer logs and report JSON, snapshot
JPEGs, HLS playlists/segments, and `demo-test.log`) and removed during final
cleanup. The temporary controller venv and `demo/logs/` were removed. Run-created
controller state (`moonlight_data/`, `msp_data/`, `scan_data/`,
`static/captures/`, `wg_keys.json`, `wg_state.json`, `connect_cache.json`) was
removed. Pre-existing untracked `controller/mesh_network.json` and
`nodes_meta.json` were left in place, as were the parallel workstream's
modified/untracked sources. No product source was edited and nothing was
committed.
