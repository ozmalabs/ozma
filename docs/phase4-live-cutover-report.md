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

## WebRTC scaling + VNC server re-validation (2026-11-25)

Date: 2026-11-25 12:34-12:48 local (01:34-01:48 UTC).
Checkout: `/home/matt/ozma/ozma`, HEAD
`a7586a34295271f1676b836dcb7e75e4debfef16`
(`feat(phase4): RFB VNC server, USB/IP control protocol, per-seat displays`).
Node binary: `target/debug/ozma-node` 0.1.0, built at the start of the run with
`CARGO_NET_OFFLINE=true cargo build -p ozma-node --bin ozma-node` (exit 0).

Controller: system Python 3.14.10 venv at
`/tmp/opencode/webrtc-vnc-reval-venv` created with `--system-site-packages`.
The missing pure-Python/ABI3 packages (`pynacl`, `mido`, `toml`,
`strawberry-graphql`, `graphql-core`, `cross-web`, `python-multipart`) were
installed **offline from the local uv cache** with
`uv pip install --offline --no-deps`. No network install, no system package,
no sudo. `gi`/GStreamer, `asyncvnc`, `numpy`, and `Pillow` came from the system
Python. TigerVNC's `vncviewer` is present, but the live client used the
controller's own `asyncvnc` for the frame/input path.

The checkout was **not frozen**. The working tree already had modified routing
docs and untracked `controller/mesh_network.json` / `nodes_meta.json`. During
the run a parallel workstream edited `Cargo.lock`, `controller/api.py`,
`controller/seats.py`, `ozma-multiseat`, `ozma-usb`, `ozma-hardware`, and
`ozma-node` sources (including new `aida64.rs`/`seats.rs`), and a parallel
`cargo` invocation rewrote `target/debug/ozma-node` at 12:47:49 local, after
the live WebRTC/VNC runs below. `ozma-node/src/webrtc.rs`,
`ozma-node/src/vnc_server.rs`, and `ozma-node/src/registration.rs` were clean
throughout. This validation does not edit product source and does not commit.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| Build current `ozma-node` | **PASS** | `cargo build -p ozma-node --bin ozma-node` exit 0; `ozma-node 0.1.0` |
| Literal `demo/run_demo.sh --webrtc`: both nodes `webrtc=running` | **PASS** | `/api/v1/connection` on 7382/7383: `"webrtc":"running"`, `webrtc_error=null` |
| Helper argv uses the probed head geometry | **PASS** | Both helpers `--width 720 --height 400 --fps 30`; log `configured=640x480 capture=720x400` |
| One real peer per node | **PASS** | 2 concurrent GStreamer peers, `connected` / ICE `completed`, 1,190 buffers / 514,080,000 bytes each, I420 720x400@30, exit 0 |
| Guest modeset no longer kills the stream | **PASS** | Capture 720x400 at 01:41:02Z, 1280x800 at 01:41:04Z; both peers kept decoding at ~30 fps through 01:41:40Z with no failure and no node error. Delivered size stayed **720x400** (nearest-neighbour downscale). |
| Controller `/api/v1/streams/<id>/webrtc` proxy | **PASS** | Peer via the 7380 proxy: `connected`, ICE `completed`, 244 buffers, I420 720x400@30, exit 0 |
| VNC registration advertises the actual bound port while listening | **PASS** | `--vnc-server-port 0` bound `127.0.0.1:38185`; registration `{"state":"configured","host":"127.0.0.1","port":38185}`; `ss` shows the node listening on 38185 |
| VNC registration retracted when the listener stops | **FAIL** | Source-failure probe bound 45211, listener stopped, but `/api/v1/connection.registration.vnc` still said `configured` port 45211 with no listener (documented startup-time gap) |
| Real RFB client frame | **PASS** | `asyncvnc` connected to `127.0.0.1:38185`; two frames decoded, shape `(800, 1280, 4)`; frame saved |
| RFB pointer/key input reaches the node input path | **PASS** | `client.keyboard.write("ab")`, `Return`, pointer move/click; node debug log `QMP input events sent on shared client` x11; guest screen OCR shows `localhost login: ab` / `Password:` |
| Controller VNC discovery | **PASS** | Controller lists `vnc1._ozma._udp.local.` with `vnc_host=127.0.0.1`, `vnc_port=38185` |
| Controller `POST /api/v1/screen/read` | **PASS (transport; OCR garbled)** | HTTP 200, 27 text regions, `screen_type=terminal`; bitmap OCR text is garbage on the 1280x800 VGA console (see gaps) |
| Controller `POST /api/v1/screen/vectors` | **PASS** | HTTP 200 `{"width":1280,"height":800,...}` in 0.23 s |
| Snapshot / HLS / input still work | **PASS** | JPEG 1280x800 direct and proxied; H.264 1280x800 segments direct and proxied; `/input/key` and `/input/type` `ok` on both nodes |
| Console switching | **SKIP (typed)** | Single-head demo advertises `display_outputs=[]`; `POST /display/select {"index":2}` → typed 404 `not_found` `available:[]` |
| `demo/run_demo.sh test` + cleanup | **PASS (suite drift)** | 73 passed / 3 failed (unchanged Connect assertions); stack, VMs, nodes, and controller stopped automatically; no leftovers |

### WebRTC scaling detail

Command (same local QEMU/controller fixture as the earlier runs):

```bash
export PATH=/tmp/opencode/nosudo:$PATH
export PYTHON=/tmp/opencode/webrtc-vnc-reval-venv/bin/python
export OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node
setsid bash demo/run_demo.sh --webrtc \
  > /tmp/opencode/webrtc-vnc-reval-2026-11-25/demo-webrtc-modeset.log 2>&1 &
```

Both nodes fitted the early QMP head exactly as before:

```text
01:40:59.109Z INFO ozma_node: fitting WebRTC helper to the capture frame size
  configured=640x480 capture=720x400
01:40:59.235Z INFO ozma_node: WebRTC helper started
  pid=Some(2071279) socket=/tmp/ozma-webrtc-vm1-2071235.sock
  shm=/dev/shm/ozma-webrtc-fb-2071235 width=720 height=400 fps=30
```

`ps` confirmed the argv on both nodes:

```text
python3 .../gst_webrtc_worker.py --sock /tmp/ozma-webrtc-vm1-2071235.sock \
  --shm /dev/shm/ozma-webrtc-fb-2071235 --width 720 --height 400 --fps 30
python3 .../gst_webrtc_worker.py --sock /tmp/ozma-webrtc-vm2-2071241.sock \
  --shm /dev/shm/ozma-webrtc-fb-2071241 --width 720 --height 400 --fps 30
```

Two genuine peers were started as soon as both nodes reported `running`
(01:41:00Z), one per node, with the stock `scripts/gate2/webrtc-client.py`
plus a temporary copy that only adds a per-second `PROGRESS` line. Capture
geometry was sampled every 2 s during the same window:

```text
01:41:02Z vm1 snapshot 720x400     01:41:02Z vm2 snapshot 720x400
01:41:04Z vm1 snapshot 1280x800    01:41:04Z vm2 snapshot 1280x800
...                                 ...
01:41:40Z vm1 snapshot 1280x800    01:41:40Z vm2 snapshot 1280x800
```

The peer progress lines straddle the modeset with no gap:

```text
PROGRESS 12:41:02 conn=connected ice=completed buffers=46  720x400
PROGRESS 12:41:03 conn=connected ice=completed buffers=76  720x400
PROGRESS 12:41:04 conn=connected ice=completed buffers=105 720x400  <-- guest now 1280x800
PROGRESS 12:41:05 conn=connected ice=completed buffers=135 720x400
...
PROGRESS 12:41:40 conn=connected ice=completed buffers=1170 720x400
```

Final peer reports:

```text
vm1: ok=true connected ice=completed, 1190 video buffers,
     514,080,000 bytes, I420 1280x800 capture delivered as 720x400@30, exit 0
vm2: ok=true connected ice=completed, 1190 video buffers,
     514,080,000 bytes, I420 1280x800 capture delivered as 720x400@30, exit 0
```

This is the requested modeset proof: the helper is fitted to the transient
720x400 boot head, the guest switches to 1280x800, and `convert_to_bgra` +
`scale_bgra_nearest` keeps both feeders and both peers alive by scaling later
frames down to the helper size. Neither node logged a dimension error, a feeder
stop, or a `webrtc_error`.

The controller proxy was exercised separately with the same client and the
controller offer URL as its target:

```bash
python3 scripts/gate2/webrtc-client.py \
  --node-url http://127.0.0.1:7382 \
  --offer-url http://localhost:7380/api/v1/streams/vm1._ozma._udp.local./webrtc \
  --duration 8
```

```text
ok=true connection_state=connected ice=completed
244 video buffers, 105,408,000 bytes, I420 720x400@30, exit 0
```

A `POST /power/reset` attempt was used first to force a guest modeset, but the
demo QEMU fixture runs with `-no-reboot`, so QEMU exited and the node lost its
capture source. That is a fixture property, not a WebRTC finding; the live boot
transition above is the actual modeset evidence.

### VNC server detail

An isolated node served the real QMP head without HLS/WebRTC contention, with
an OS-assigned port (`0`):

```bash
RUST_LOG='info,ozma_input=debug' ./target/debug/ozma-node \
  --name vnc1 --profile vm \
  --qmp-socket /tmp/ozma-vm1.qmp \
  --hid-udp-port 17331 --http-port 17382 \
  --capture-provider qmp --input-provider qmp \
  --vnc-server --vnc-server-port 0 \
  --cap hid,video
```

Listener and real endpoint:

```text
INFO ozma_node::vnc_server: RFB 3.8 VNC server listening
  bind=127.0.0.1:38185 loopback=true security="none" encoding="raw"
INFO ozma_node: RFB 3.8 VNC server is serving the first capture source
  bind=127.0.0.1:38185 advertised_host=127.0.0.1 advertised_port=38185
INFO ozma_node::vnc_server: VNC capture source published its first frame;
  ServerInit geometry fixed width=1280 height=800
```

Registration and the kernel agree on the ephemeral port:

```text
GET /api/v1/connection.registration
{"vnc":{"state":"configured","host":"127.0.0.1","port":38185}, ...}

ss -lntp
LISTEN 127.0.0.1:38185 users:(("ozma-node",pid=2081829,fd=10))
```

The unmodified controller discovered the same endpoint through mDNS:

```text
GET /api/v1/nodes
[("vnc1._ozma._udp.local.", "127.0.0.1", 38185)]
```

Real client (controller's `asyncvnc`) connected, decoded two full frames, sent
keys and pointer, decoded a third frame, and stayed connected:

```text
connected=true  frames=(800,1280,4)  changed_pixels f1->f2 = 0
keys "ab" + Return + pointer (640,400) + left click
changed_pixels f2->f3 = 325 (bbox x=0..150, y=626..655, the guest text line)
still_connected_after_input=true
```

The input path is confirmed twice. The node's QMP adapter logged 11 successful
batches during the client connection:

```text
DEBUG ozma_input::qmp: QMP input events sent on shared client count=1
DEBUG ozma_input::qmp: QMP input events sent on shared client count=2
... (11 lines at 01:44:58Z)
```

and the guest screen (captured through the same VNC socket) visibly reacted:

```text
localhost login: ab
Password: _
```

(`tesseract` on the controller's own saved frame reads the same line; this is
independent of the controller's bitmap OCR path.)

Controller consumers:

```text
POST /api/v1/screen/read       → 200, 27 text_regions, screen_type=terminal
POST /api/v1/screen/read-prompt→ 200
POST /api/v1/screen/find-button→ 200 {"found":false}
POST /api/v1/screen/vectors    → 200 {"width":1280,"height":800,
                                     "rectangles":[],"lines":[]}
```

The screen/read **transport** is healthy, but the returned OCR text is garbage
(e.g. `*Nk*Wf9MUH**'`). `screen_reader` auto-detects a terminal and uses its
bitmap font matcher (`ocr_method=bitmap`) on the 1280x800 anti-aliased VGA
console; the same frame read by `tesseract` correctly yields the Alpine boot
text and login line. This is a `screen_reader` decoder limitation, not a VNC
frame/transport failure. The testbench `_capture_results` path uses the same
`ScreenReader.read_node_screen`, so it inherits that limitation; no benchmark
harness was run.

**Registration retraction gap (live repro).** A second probe enabled
`--vnc-server` on an X11 source with no display. The listener bound
`127.0.0.1:45211`, the ffmpeg source ended, and the listener stopped:

```text
01:46:23.766Z RFB 3.8 VNC server listening bind=127.0.0.1:45211
01:46:23.766Z advertised_host=127.0.0.1 advertised_port=45211
01:46:24.138Z VNC capture source ended; the VNC listener is stopping
```

But `/api/v1/connection.registration.vnc` still returned
`{"state":"configured","host":"127.0.0.1","port":45211}` and `ss` had no
listener on 45211. This is exactly the startup-time registration limitation in
`docs/phase4-vnc-server-report.md`: the advertised endpoint is correct while
the listener runs, but a later listener stop does not retract the mDNS/TXT or
the `/api/v1/connection` registration object.

### Snapshot, HLS, and input

Measured on the same literal `--webrtc` run (after the guest settled):

- `GET /display/snapshot` 200 `image/jpeg`, 96,470 / 96,488 bytes, decoded
  **1280x800** on vm1/vm2; controller
  `/api/v1/streams/vm1.../snapshot` also 200 `image/jpeg` 1280x800.
- Local `GET /stream/stream.m3u8` 200; last segment 200
  `video/vnd.dlna.mpeg-tts` 100,392 bytes, `ffprobe` `h264,1280,800` on both.
  Controller `/api/v1/streams/vm1.../hls/stream.m3u8` 200
  `application/vnd.apple.mpegurl`.
- `/input/key {"keycode":30,"down":true|false}` → `{"ok":true,"sink":"qmp"}`
  and `/input/type {"text":"ab"}` → `{"ok":true,"events":4}` on both nodes.
- Controller activation then `POST /api/v1/paste` → 200
  `{"chars_sent":2,"chars_skipped":0}`.
- `POST /display/select {"index":2}` → typed 404 `not_found`,
  `available:[]` (single-head demo, unchanged).

### `demo test` cleanup

```bash
PYTHON=/tmp/opencode/webrtc-vnc-reval-venv/bin/python \
OZMA_NODE_BIN=/home/matt/ozma/ozma/target/debug/ozma-node \
PATH=/tmp/opencode/nosudo:$PATH \
bash demo/run_demo.sh test
```

Exit 1 from the unchanged controller-only Connect drift:

```text
FAIL  AI setups used: 0
FAIL  noise_cancellation blocked on free tier
FAIL  reason mentions upgrade:
73 passed, 3 failed, 0 skipped
SOME TESTS FAILED
```

The harness stopped the failing stack without manual intervention:
`Stopping Ozma demo...` killed the controller and both nodes, `Stopping VMs...`
killed both QEMU processes, then `Done.` Post-run checks found no
`ozma-node`, controller, QEMU, helper, or ffmpeg process; no listeners on
7332/7333, 7380, 7382/7383, 7384, 5901, or 5922; and no demo pidfiles or QMP
sockets. Cleanup is **PASS**.

### Remaining gaps after this re-validation

1. **The literal `--webrtc` demo now stays alive but is downgraded to 720x400.**
   The fit probe still samples the transient boot head, so the guest's final
   1280x800 head is nearest-neighbour downscaled to 720x400 for the life of the
   node. This satisfies "modeset must not kill the stream", but native 1280x800
   still needs a stable-size probe, a re-fit/restart on capture-format change,
   or deferred helper start. Not applied here.
2. **WebRTC geometry is still absent from the status API.** `/api/v1/connection`
   reports only `webrtc: running|failed` and `webrtc_error`; width/height are
   visible only in the node log, helper argv, and peer caps.
3. **VNC registration is startup-time, not live.** The live repro above shows a
   stopped listener still advertised with `state=configured`. A retraction path
   (or a live listener probe on `/api/v1/connection`) is still missing.
4. **Controller screen OCR is wrong for this VGA console.** The transport
   delivers the correct 1280x800 frame, but `screen_reader` picks its bitmap
   terminal decoder and returns garbage. Tesseract reads the same frame
   correctly, so the fix is in screen-type/OCR selection, not in RFB.
5. **VNC remains security None + Raw only**, no auth/TLS/compression/resize;
   one shared input sink; one capture source. Unchanged from the VNC report.
6. **WebRTC audio is still unconfigured**
   (`webrtc_audio_configured=false`, `webrtc_audio_source=null`); the peers
   negotiated video only.
7. **The checkout and binary moved during validation.** `target/debug/ozma-node`
   was rewritten by a parallel cargo invocation at 12:47:49 local, after the
   live WebRTC/VNC runs; a frozen-tree rerun would remove this caveat.
8. **Controller test drift is unchanged** (the three Connect failures above),
   unrelated to the Rust node, WebRTC, or VNC.

### Artifacts and cleanup

Artifacts were written to `/tmp/opencode/webrtc-vnc-reval-2026-11-25/`:
demo start/stop and `demo-test.log`, mode-set timeline, per-second peer progress
logs and report JSON, connection JSON, helper process/argv captures, snapshot
JPEGs, HLS playlist/segment files, controller screen-read/vectors JSON, VNC
client frames and result JSON, and the VNC node/controller logs. The temporary
controller venv was `/tmp/opencode/webrtc-vnc-reval-venv`. Both the artifact
directory and the venv were removed during final cleanup, as were `demo/logs/`
and the controller state created by these runs (`moonlight_data/`, `msp_data/`,
`scan_data/`, `static/captures/`, `wg_keys.json`, `wg_state.json`,
`connect_cache.json`). Pre-existing untracked `controller/mesh_network.json`
and `nodes_meta.json`, plus the parallel workstream's modified/untracked
sources, were left untouched. No product source was edited by this validation
and nothing was committed. The only tracked files this validation wrote are
this report and the VNC report note; the other tracked modifications in the
working tree belong to the parallel workstream.

## WebRTC settled fit probe (2026-11-25, code change; not live-validated)

This change addresses gaps 1 and 2 in "Remaining gaps after this re-validation"
above. It is covered by unit tests only: no demo, QEMU guest, helper, or peer
was run for this change, so it does **not** claim the literal `--webrtc` demo
is fixed end to end.

### Probe behaviour

- `--webrtc-fit-capture` remains opt-in. Without it, a declared capture size
  that differs from `--webrtc-width/--webrtc-height` is still a typed
  `frame dimensions ... do not match` error before the helper spawns.
- With the flag, `ozma-node` only probes when discovery geometry is unknown
  (`0x0`, as with QMP/VNC before the first screendump). A known discovery size
  is used directly, as before; no frames are consumed.
- An unknown probe now requires the observed size to stay unchanged for a
  settle window before accepting it, instead of latching the first frame. The
  default is **1500 ms**, tunable with `--webrtc-settle-ms` /
  `OZMA_WEBRTC_SETTLE_MS` (clamped to 0-5000 ms; `0` accepts the first usable
  frame). The whole probe is hard-bounded at **8 s**, including frame waits.
- If the size never settles, or the source ends/errors/times out after frames
  were seen, the **last observed non-zero size** is used and
  `webrtc_geometry.settled` is `false`. The existing nearest-neighbour scaling
  fallback still rescales later frames to the helper target, so a modeset does
  not kill the stream.
- If no usable frame arrives at all, the configured `--webrtc-width/height`
  stays in place and the helper's own dimension check still fails honestly.
- The probe consumes the frames it reads; they are not replayed to the feeder,
  so the stream starts after the settle wait.

### Status fields

`GET /api/v1/connection` keeps the existing `webrtc` string and
`webrtc_error`, and adds an additive `webrtc_geometry` object when a helper is
configured:

```json
"webrtc_geometry": {
  "width": 1280,
  "height": 800,
  "scale_to_fit": true,
  "fitted": true,
  "capture_width": 1280,
  "capture_height": 800,
  "settled": true
}
```

- `width`/`height` are the helper target written to shared memory.
- `scale_to_fit` is the opt-in runtime rescale flag.
- `fitted` is true when the probe changed the configured helper size.
- `capture_width`/`capture_height`/`settled` are present only when the probe
  ran; `settled=false` means the bounded fallback was used.
- `webrtc_geometry` is omitted when WebRTC is not configured, so consumers
  that read only `webrtc`/`webrtc_error` are unaffected. For the opt-in
  multi-console manager the object describes the active console's helper.

### Tests and checks run

- `cargo test -p ozma-node --lib` — 479 passed, 3 ignored (including the
  existing GStreamer helper tests).
- `cargo check -p ozma-node --all-targets` — clean.
- `rustfmt --edition 2021 --check` on
  `ozma-node/src/{main,webrtc,controller_compat}.rs` — clean.

New unit tests: a 720x400-then-1280x800 scripted source settles on 1280x800;
an alternating source never settles and returns a bounded last-size fallback;
fit-off still produces the typed dimension error; status exposes configured
and probed geometry (including JSON field shape).

### Remaining WebRTC gaps

1. **No live re-run yet.** The demo/QMP path, helper argv, peer SDP caps, and
   `/api/v1/connection` were not exercised against a running guest with this
   binary. A frozen-tree `demo/run_demo.sh --webrtc` run is still required to
   confirm native 1280x800 end to end.
2. **No re-fit after startup.** If the guest modesets after the settle window,
   the helper target stays at the probed size and later frames are
   nearest-neighbour scaled. A capture-format-change restart (or deferred
   helper start) is still future work. A boot mode that persists longer than
   the settle window can still be latched; raise `--webrtc-settle-ms` (up to
   the 5000 ms bound) for such a guest.
3. **The multi-console WebRTC manager does not run this probe.** It uses the
   per-console geometry supplied by the display selection and reports that
   geometry in status.
4. **Probed frames are discarded.** A future change could replay the last
   probe frame as the first feeder frame to remove the initial gap.
5. **Scaling remains nearest-neighbour**, with no aspect-ratio or colour-space
   correction.
6. **WebRTC audio and the GStreamer/Python helper are unchanged**; there is
   still no native WebRTC implementation.

## Live regression at e1b6f57 (2026-11-25)

Date: 2026-11-25, 13:22-13:37 local (02:22-02:37 UTC).
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64.
Checkout: live tree `/home/matt/ozma/ozma` was **dirty with unrelated AIDA64
work**, so this run used a detached worktree at
`/tmp/opencode/live-reg-e1b6f57`.
HEAD: `e1b6f570f2093da7e113fc75ac4662138b958c98`
(`feat(phase4): USB/IP data wiring, settled WebRTC fit, audit refresh`).
Node binary: worktree `target/debug/ozma-node` 0.1.0, built at run start with
`CARGO_NET_OFFLINE=true cargo build -p ozma-node --bin ozma-node` (exit 0,
48.44 s).

This was validation only. No product source was changed and nothing was
committed; the live checkout received only this report section.

Environment: controller venv
`/tmp/opencode/live-reg-e1b6f57-venv` (system Python 3.14.10 with
`--system-site-packages`; `mido`, `pynacl`, `toml`, `strawberry-graphql`,
`graphql-core`, `cross-web` installed **offline** from the local uv cache). QEMU
with `/dev/kvm`, ffmpeg, avahi, PipeWire, Playwright Chromium, `gi`/GStreamer,
`asyncvnc`, and TigerVNC were available. A PATH `sudo` shim
(`/tmp/opencode/nosudo/sudo`) kept the optional v4l2loopback step non-fatal. The
worktree's `demo/cache` was symlinked to the existing Alpine 3.21.3 ISO cache;
no network fetch was performed.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| Build `ozma-node` at HEAD | **PASS** | offline `cargo build` exit 0; `ozma-node 0.1.0` |
| Default demo (no `--rust`) | **PASS** | both Rust VM nodes + Python controller started; log `Backend: rust (default: rust)` |
| mDNS discovery by unmodified controller | **PASS** | `/api/v1/nodes` listed `vm1`/`vm2`; no registration URL |
| Health and node detail | **PASS** | `/health` 200 `ok:true` on 7382/7383; `/api/v1/nodes/<id>` 200 with ports, caps, audio, `stream_url` |
| Dashboard | **PASS** | `/dashboard.html` title `Ozma — Dashboard`, `node-count=2`, 2 `.node-card[data-id]`, 2 scenario cards; one expected 503 from `/api/v1/supervisor/status` |
| Local HLS | **PASS** | `stream.m3u8` 200; `seg_00033.ts` 200, 100,392 B, `video/vnd.dlna.mpeg-tts` |
| Controller HLS proxy | **PASS** | proxied m3u8 200; segment 200, 100,392 B, `video/mp2t` |
| Snapshots direct + proxy | **PASS** | JPEG 200, 1280x800, ~96 KB on both nodes and both proxies |
| QMP input | **PASS** | `/input/key` 200 `{"ok":true,"sink":"qmp"}` down/up; `/input/type` 200, 10 events; both nodes |
| Controller paste | **PASS** | after `vm1` activation, `POST /api/v1/paste` 200 `chars_sent=4`, `chars_skipped=0` |
| Switching E2E | **PASS** | `tests/test_e2e_switching.py --no-audio`: 24 PASS, 0 FAIL; five rapid switches ended on `vm1` |
| Graceful shutdown / no orphans | **PASS** | `SIGTERM` 02:24:35Z; node shutdown log 7.6-9.8 ms later; no processes, listeners, QMP sockets or pidfiles left |
| `demo test` suite | **FAIL (known)** | 73 passed, 3 failed, 0 skipped: F25 `AI setups used`, F26 `noise_cancellation blocked`, F26 `reason mentions upgrade` |
| `demo test` cleanup on failure | **PASS** | failing suite still ran `Stopping Ozma demo...`; nodes/controller/VM/QEMU stopped, no leftovers |
| `--webrtc` settled-fit picks native 1280x800 | **FAIL** | both nodes chose **720x400** with `webrtc_geometry.settled=true`; expectation was 1280x800 |
| Two real peers, one per node | **PASS** | `connected` / ICE `completed`, 598 buffers / 258,336,000 B each, I420 720x400@30 |
| Controller-proxy peer | **PASS** | `connected` / ICE `completed`, 362 buffers / 156,384,000 B, I420 720x400@30 |
| Guest modeset does not kill streams | **PASS** | capture 720x400 -> 1280x800 at 02:28:59Z; both peers advanced 54 -> 83 -> 113 buffers with no error |
| VNC registration advertises bound port | **PASS** | `--vnc-server-port 0` bound `127.0.0.1:37267`; registration and `ss` agree |
| Real VNC frame + input | **PASS** | `asyncvnc` frame `(800,1280,4)`; after keys/pointer 341 changed pixels, bbox `(0,674)-(150,703)`; 29 QMP input batches; still connected |
| Controller VNC discovery | **PASS** | `/api/v1/nodes` -> `vnc1` `vnc_host=127.0.0.1`, `vnc_port=37267` |
| Controller `screen/read` | **PASS (transport)** | 200, 27 regions, `screen_type=terminal`; bitmap OCR text is garbled as previously documented |
| Controller `screen/vectors` | **PASS** | 200, `{"width":1280,"height":800,"rectangles":[],"lines":[]}` in 0.19 s |
| VNC registration retracts when listener stops | **PASS** | source ended 02:32:05Z; `/api/v1/connection.registration.vnc` became `not-configured` while node stayed alive |
| VNC mDNS withdrawal after node stop | **PASS (cache note)** | zeroconf browse returned no `_ozma._udp` services after stop; `avahi-browse` showed cached entries, and the `vncext` T-shirt TXT kept the dead port until node stop (known startup-time gap) |
| Clean-host `--host` (first attempt) | **FAIL (environment)** | ENOSPC: `/tmp` tmpfs 16 G 100 %; `rustc ... No space left on device (os error 28)` |
| Clean-host `--host` (rerun, /home target) | **PASS** | 20 passed, 0 failed, 0 skipped; `ozma-node --lib` 473 passed/3 ignored; `ozma-softnode` 128 passed; synthetic `/health` PASS |
| Clean-host `--container --full` | **SKIP** | no cached `localhost/ozma-clean-host:1.95` (`podman image exists` exit 1); rebuild requires network `apt-get`, which was prohibited |

### Default demo detail

```bash
export PATH=/tmp/opencode/nosudo:$PATH
export PYTHON=/tmp/opencode/live-reg-e1b6f57-venv/bin/python
export OZMA_NODE_BIN=/tmp/opencode/live-reg-e1b6f57/target/debug/ozma-node
bash demo/run_demo.sh
```

Both nodes were discovered without a registration URL, and the dashboard
rendered two node cards and two scenario cards. HLS media sequence reached 33;
the direct segment was `video/vnd.dlna.mpeg-tts`, the controller proxy returned
`video/mp2t` for the same 100,392-byte segment. Direct and proxied snapshots
were both 1280x800. QMP key down/up and `Ozma` typing returned `ok` on both
nodes; paste reported `chars_sent=4`.

Shutdown killed the controller/node PIDs with `SIGTERM`; the node logs show:

```text
02:24:35.144743Z shutdown requested; stopping node tasks signal="SIGTERM"
02:24:35.154508Z Ozma node shutting down
```

### `demo test` detail

`bash demo/run_demo.sh test` ended with **73 passed, 3 failed, 0 skipped** and
exit 1. The three failures are the unchanged controller-only Ozma Connect
assertions (`AI setups used: 0`, `noise_cancellation blocked on free tier`,
`reason mentions upgrade:`); no Rust-node check failed. Because the suite
failed, cleanup was exercised directly: the parent run still printed
`Stopping Ozma demo...`, killed both nodes and the controller, stopped both
VMs, and left no demo processes or listeners.

### WebRTC settled-fit detail (the regression)

The literal default `--webrtc` run fitted both helpers to the **boot** head:

```text
02:28:56.727Z WebRTC fit probe resolved the capture geometry
  width=720 height=400 frames=45 settled=true settle_ms=1500
02:28:56.727Z fitting WebRTC helper to the capture frame size
  configured=640x480 capture=720x400 settled=Some(true)
```

`/api/v1/connection` on both nodes returned:

```json
"webrtc_geometry": {
  "width": 720, "height": 400, "scale_to_fit": true, "fitted": true,
  "capture_width": 720, "capture_height": 400, "settled": true
}
```

Helper argv confirmed `--width 720 --height 400 --fps 30` on both processes.
The Alpine guest then modeset to 1280x800:

```text
02:28:57.637Z vm1=(720,400)  vm2=(720,400)
02:28:59.061Z vm1=(720,400)  vm2=(720,400)
02:28:59.774Z vm1=(1280,800) vm2=(1280,800)   <-- modeset
```

Two genuine GStreamer peers were already connected and spanning that change:

```text
13:28:58 conn=connected ice=completed buffers=24  caps=I420 720x400@30
13:28:59 conn=connected ice=completed buffers=54  caps=I420 720x400@30
13:29:00 conn=connected ice=completed buffers=83  caps=I420 720x400@30
...
peer vm1: ok=true, 598 buffers, 258,336,000 bytes, exit 0
peer vm2: ok=true, 598 buffers, 258,336,000 bytes, exit 0
```

Both node peers and the separately rerun controller-proxy peer reported
`connected` / ICE `completed` and 720x400 I420 caps. The modeset therefore did
**not** kill either stream; later 1280x800 frames were scaled to the helper's
720x400 target. A first proxy attempt launched before the controller socket was
listening failed with `ConnectionRefusedError`; it passed when rerun against the
ready proxy. That is a launch-timing artefact, not a proxy regression.

Root cause of the geometry failure: the boot mode is stable for roughly 2.5-3 s,
longer than the 1500 ms default settle window, so the probe accepts 720x400 as
"settled" before the modeset. The launcher does not pass a settle override.
Diagnostic runs at the same HEAD confirm the mechanism itself:

```text
--webrtc-settle-ms 5000: width=1280 height=800 frames=232 settled=false
  (native size chosen by the 8 s bounded fallback)
--webrtc-settle-ms 3500: width=1280 height=800 frames=203 settled=true
```

So native 1280x800 **with** `settled=true` is reachable with
`OZMA_WEBRTC_SETTLE_MS=3500` (or `--webrtc-settle-ms 3500`), but at this HEAD the
default demo still latched 720x400. This is the "boot mode persists longer than
the settle window" gap noted above, reproduced live.

**Follow-up fix:** the node default settle window is now 4000 ms (still clamped
to 0-5000 and within the 8 s probe bound), and `demo/run_demo.sh --webrtc`
passes `--webrtc-settle-ms 4000` explicitly. A live rerun on the next commit is
required to confirm native 1280x800 by default; the 3500 ms diagnostic already
proves the mechanism.

### VNC server detail

An isolated node served the real QMP head with an OS-assigned port:

```bash
RUST_LOG='info,ozma_input=debug' ./target/debug/ozma-node \
  --name vnc1 --profile vm --qmp-socket /tmp/ozma-vm1.qmp \
  --hid-udp-port 17331 --http-port 17382 \
  --capture-provider qmp --input-provider qmp \
  --vnc-server --vnc-server-port 0 --cap hid,video
```

```text
02:30:56.630Z RFB 3.8 VNC server listening bind=127.0.0.1:37267
02:30:56.630Z advertised_host=127.0.0.1 advertised_port=37267
02:30:56.664Z ServerInit geometry fixed width=1280 height=800
GET /api/v1/connection -> registration.vnc
  {"state":"configured","host":"127.0.0.1","port":37267}
ss -lntp: LISTEN 127.0.0.1:37267 users:(("ozma-node",pid=...,fd=10))
```

`asyncvnc` decoded a 1280x800 frame, sent `ab`, Return, a pointer move and a
left click, and decoded a changed frame; the node logged 29
`QMP input events sent on shared client` batches. The controller discovered the
same endpoint through mDNS and returned it from `/api/v1/nodes`.
`POST /api/v1/screen/read` returned 200 with 27 terminal regions (the known
garbled bitmap OCR), and `POST /api/v1/screen/vectors` returned 200 at
1280x800.

**Retraction.** A second desktop node with `--capture-provider x11
--x11-display :99` bound `127.0.0.1:38845`, its source ended, the listener
stopped, and the node stayed alive. `/api/v1/connection.registration.vnc`
changed to `{"state":"not-configured"}`, so the live-listener retraction added
before this HEAD works. The mDNS TXT record for that node still carried
`vnc_port=38845` until the node exited; after both VNC nodes stopped, a
zero-config browse of `_ozma._udp.local.` returned no services. `avahi-browse`
continued to show cached records for a short while, which is cache behaviour,
not a live responder. The controller's in-memory node list also lagged, as
before.

### Clean-host detail

The first `--host` run used the default target under the worktree on the 16 G
`/tmp` tmpfs and failed when the tmpfs filled:

```text
error: failed to write .../target/clean-host/debug/deps/.../lib.rmeta:
  No space left on device (os error 28)
rustc-LLVM ERROR: IO failure on output stream: No space left on device
```

After removing that build target, the rerun used
`OZMA_CLEAN_HOST_TARGET=/home/matt/.cache/ozma-clean-host-live-reg` (on `/home`)
and passed:

```text
PASS  cargo build -p ozma-node -p ozma-softnode --bins
PASS  cargo test -p ozma-node --lib        (473 passed, 3 ignored)
PASS  cargo test -p ozma-softnode          (128 passed)
PASS  ozma-node desktop profile answered /health on port 32204
20 passed, 0 failed, 0 skipped
RESULT: PASS
```

The container path was skipped: no tagged cached image exists
(`localhost/ozma-clean-host:1.95`), and building it would require the
`apt-get` network step in `scripts/clean-host/Containerfile`, which the task
prohibited.

### Regressions and findings

1. **Settled-fit still latches the boot head in the literal demo.** Both nodes
   report 720x400 with `settled=true`; native 1280x800 requires a settle window
   of at least ~3.5 s. Peers and scaling remain healthy, so this is a geometry
   regression against the intended expectation, not a stream-death regression.
2. **VNC mDNS TXT is still startup-time only.** The live registration object
   retracts correctly, but the advertised TXT port is stale until node stop.
3. **`demo test` still has three controller-only Ozma Connect failures.** The
   failing-suite cleanup path now passes.
4. No node crash, orphan, leaked listener, or default-path regression was
   observed.

### Cleanup status

Final checks after the run found no `ozma-node`, QEMU, controller,
`gst_webrtc_worker.py` or HLS ffmpeg processes; no listeners on
7380/7332/7333/7382/7383/5901/5922 or the VNC ports; no
`/tmp/ozma-vm*.qmp`, demo pidfiles, or `/dev/shm/ozma-*`; and no live
`_ozma._udp` responders. Controller state created by these runs lived only in
the detached worktree. The worktree, its venv, the reusable artifacts, and the
clean-host target were removed after writing this section. Pre-existing
`/tmp/ozma-webrtc-*` empty directories from earlier sessions were left
untouched.

## Final validation at 7b9c0a1 (2026-11-25)

Date: 2026-11-25, 13:52-14:10 local (02:52-03:10 UTC).
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64.
Checkout: the live tree was again **dirty with unrelated AIDA64 work**, so this
run used a detached worktree at `/tmp/opencode/final-7b9c0a1`.
HEAD: `7b9c0a168838fe21a52b11526da39383070d8b22`
(`feat(phase4): media auto-start, packaging, OCR fix, settled-fit default`).
Node binary: worktree build with
`CARGO_NET_OFFLINE=true cargo build -p ozma-node --bin ozma-node` (exit 0,
45.31 s). Controller venv:
`/tmp/opencode/final-venv313` (offline `uv` install of
`controller/requirements.txt` plus `pytest`).

This was validation only. No product source was changed and nothing was
committed; the live checkout received only this report section. The demo's
`demo/cache` was symlinked to the existing Alpine 3.21.3 ISO cache.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| Build `ozma-node` at HEAD | **PASS** | offline `cargo build` exit 0 in 45.31 s; `ozma-node 0.1.0` |
| Literal `demo/run_demo.sh --webrtc` | **PASS** | both Rust VM nodes + controller started; helper argv included `--webrtc-settle-ms 4000` |
| Settled fit at native 1280x800 | **PASS** | both nodes `width=1280 height=800 settled=true settle_ms=4000`; `/api/v1/connection.webrtc_geometry` matched, not 720x400 |
| Two real direct GStreamer peers | **PASS** | vm1 356 buffers / 546,816,000 B; vm2 351 / 539,136,000 B; `connected`, ICE `completed`, I420 1280x800@30, exit 0 |
| Controller-proxy peer | **PASS** | 356 buffers / 546,816,000 B; `connected`, ICE `completed`, I420 1280x800@30, exit 0 |
| Peer stream stability across boot/modeset | **PASS (caveat)** | the 4 s settle put helper start after the guest boot modeset, so peers attached to the native head; all three peers held 1280x800 for the full 12 s session with no reset or geometry change |
| VNC server + real client | **PASS** | node bound `127.0.0.1:35983`; `asyncvnc` decoded `(800,1280,4)`, input changed 325 pixels (bbox `(0,626)-(182,655)`), 19 QMP input batches |
| VNC registration advertises bound port | **PASS** | node registration `{"state":"configured","host":"127.0.0.1","port":35983}`; `ss` showed the same listener PID |
| Controller VNC discovery | **PASS** | `/api/v1/nodes` returned `vnc1._ozma._udp.local.` with `vnc_host=127.0.0.1`, `vnc_port=35983` |
| Controller `screen/read` (OCR fix) | **PASS** | HTTP 200, `ocr_method=tesseract`, `ocr_status=ok`, `ocr_confidence=0.82`, 235 text regions, readable Alpine console text |
| Controller `screen/vectors` | **PASS** | HTTP 200, `{"width":1280,"height":800,"rectangles":[],"lines":[]}` in 0.39 s |
| VNC registration retracts when source stops | **PASS** | after the QMP source ended, registration became `{"state":"not-configured"}` while `/health` stayed 200 |
| `demo test` suite | **FAIL (known + 2 environment)** | 70 passed, 5 failed, 0 skipped: the three known Connect failures plus F19/F20 after the controller stalled on physical-capture OCR |
| `demo test` cleanup on failure | **PASS** | failing suite still printed `Stopping Ozma demo...`, killed both nodes/controller, stopped both VMs, no leftovers |
| Clean-host `--host` | **PASS** | 22 passed, 0 failed, 0 skipped; `ozma-node --lib` 479 passed/3 ignored; `ozma-softnode` 128 passed; synthetic `/health` PASS |
| Clean-host `--container --full` | **FAIL (stale cache)** | dangling cached image existed but its `check-deps` failed `libxdo is missing`; rebuilding needs the network `apt-get` step |
| Process/socket/temp cleanup | **PASS** | no demo processes, listeners, QMP sockets, pidfiles, or `/dev/shm/ozma-*` remain |

### Settled geometry and peers

Both nodes resolved the native head during the 4000 ms settle window, before
the helper was spawned:

```text
03:00:44.457Z INFO ozma_node: WebRTC fit probe resolved the capture geometry
  width=1280 height=800 frames=224 settled=true settle_ms=4000
03:00:44.457Z INFO ozma_node: fitting WebRTC helper to the capture frame size
  configured=640x480 capture=1280x800 settled=Some(true)

03:00:44.412Z INFO ozma_node: WebRTC fit probe resolved the capture geometry
  width=1280 height=800 frames=222 settled=true settle_ms=4000
03:00:44.412Z INFO ozma_node: fitting WebRTC helper to the capture frame size
  configured=640x480 capture=1280x800 settled=Some(true)
```

`/api/v1/connection` on 7382 and 7383 returned the same geometry:

```json
"webrtc_geometry": {
  "width": 1280, "height": 800, "scale_to_fit": true, "fitted": true,
  "capture_width": 1280, "capture_height": 800, "settled": true
}
```

The two helper processes were launched as
`--width 1280 --height 800 --fps 30`. Two direct
`scripts/gate2/webrtc-client.py` peers and one peer through
`/api/v1/streams/vm1._ozma._udp.local./webrtc` ran concurrently
(03:00:59-03:01:12Z):

```text
vm1 direct:  ok=true conn=connected ice=completed 356 buffers / 546,816,000 B
vm2 direct:  ok=true conn=connected ice=completed 351 buffers / 539,136,000 B
vm1 proxy:   ok=true conn=connected ice=completed 356 buffers / 546,816,000 B
caps: video/x-raw I420 1280x800 progressive 30/1
```

All three client processes exited 0. Because the 4 s settle intentionally
waits past the Alpine boot modeset, the literal run no longer has a mid-stream
boot transition: peers attach to the already-native 1280x800 head and show no
stream reset, renegotiation, or geometry change for the whole session.

### VNC and OCR

An isolated node served the real QMP head through `--vnc-server
--vnc-server-port 0` on HTTP 17382:

```text
RFB 3.8 VNC server listening bind=127.0.0.1:35983 loopback=true
RFB 3.8 VNC server is serving the first capture source
  advertised_host=127.0.0.1 advertised_port=35983
VNC capture source published its first frame; ServerInit geometry fixed width=1280 height=800
```

`ss` and the node registration agreed on `127.0.0.1:35983`. A real `asyncvnc`
client decoded a `(800,1280,4)` frame; typing `ab`, Return, a pointer move and a
left click changed 325 pixels in `(0,626)-(182,655)` and produced 19
`QMP input events sent on shared client` batches.

The controller discovered `vnc1._ozma._udp.local.` at that port. OCR is now
readable rather than the earlier bitmap garbage:

```json
"screen_type": "terminal",
"ocr_method": "tesseract",
"ocr_status": "ok",
"ocr_confidence": 0.82,
"text_regions": 235
```

`raw_text` (1,275 chars) included `OpenRC`, `Linux 6.12.13-0-virt (x86_64)`,
`localhost`, and `login:`. `POST /api/v1/screen/vectors` returned
`{"width":1280,"height":800,"rectangles":[],"lines":[]}` in 0.39 s.

When the QMP capture source was stopped, the listener retracted:

```text
before: {"state":"configured","host":"127.0.0.1","port":35983}
after:  {"state":"not-configured"}
node /health: 200
```

The controller's cached node row still carried the old `vnc_port=35983`, the
same in-memory discovery lag noted in earlier sections.

### `demo test` and cleanup

`bash demo/run_demo.sh test` ended with **70 passed, 5 failed, 0 skipped** and
exit 1. The three expected controller-only Ozma Connect failures reproduced:
`AI setups used: 0`, `noise_cancellation blocked on free tier`, and
`reason mentions upgrade:`.

Two extra failures appeared on this host and are environmental, not Rust-node
regressions: F19 `GET /api/v1/macros returns data` and F20 `automation script
accepted`. The controller had a physical display capture source
(`hdmi-0`, Creative Live! Cam Sync 4K on `/dev/video0`); the F18 OCR call on
that source blocked long enough for the nodes to be marked offline
(`14:04:44 Node stale, marking offline: vm1/vm2`) and for the subsequent
requests to time out. The macro record start/stop calls themselves succeeded.
No node, WebRTC, VNC, or switching assertion failed.

The failing suite still exercised cleanup: it printed `Stopping Ozma demo...`,
killed controller and both node PIDs, stopped both QEMU VMs, and left no demo
processes or listeners.

### Clean-host

`scripts/clean-host/run.sh --host` passed with its target redirected to
`/home/matt/.cache/ozma-clean-host-final` (the worktree `/tmp` tmpfs is too
small for a fresh target):

```text
PASS  cargo build -p ozma-node -p ozma-softnode -p ozma-multiseat --bins
test result: ok. 479 passed; 0 failed; 3 ignored
PASS  cargo test -p ozma-node --lib
test result: ok. 128 passed; 0 failed
PASS  cargo test -p ozma-softnode
PASS  ozma-node desktop profile answered /health on port 30056
22 passed, 0 failed, 0 skipped
RESULT: PASS
```

`--container --full` was attempted because a dangling cached image from
2026-11-24 20:09 UTC existed (`9a91e138532b`, history matching the harness
Containerfile except the new `libxdo-dev`). It failed before building:

```text
FAIL  libxdo is missing (enigo link dependency for ozma-hid tests)
FAIL  native build dependencies
RESULT: FAIL
```

A rebuild would need the `apt-get` network step in
`scripts/clean-host/Containerfile`, so the container run is recorded as a
stale-cache failure rather than a product failure. The temporary
`localhost/ozma-clean-host:1.95` tag was removed during cleanup.

### Cleanup status

After the runs there were no `ozma-node`, QEMU, controller, WebRTC helper,
peer-client, or clean-host processes; no listeners on
7380/7332/7333/7382/7383/5901/5922/17382 or the VNC port; no
`/tmp/ozma-vm*.qmp`, demo pidfiles, or `/dev/shm/ozma-*`; and no live
`_ozma._udp` responders. Controller state created by these runs lived only in
the detached worktree and was removed with it. The worktree, venvs, node build
target, clean-host targets, and temporary `/tmp/opencode/final-artifacts`
evidence directory were removed after this section was written. Pre-existing
`/tmp/ozma-webrtc-*` directories from earlier sessions were left untouched.

## Media validation at e40e62c (2026-11-25)

Date: 2026-11-25, 16:10-16:30 local (05:10-05:30 UTC).
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64.
Checkout: the live tree was **dirty with unrelated AIDA64 work**, so this run
used a detached worktree at `/tmp/opencode/media-val-e40e62c`.
HEAD: `e40e62c955619a79e362188e330c5391d8d80549`
(`feat(phase4): node MJPEG video, mesh packet pump, USB media daemon`).
Node binary: offline `cargo build -p ozma-node --bin ozma-node` (exit 0,
45.55 s; `ozma-node 0.1.0`). Controller venv:
`/tmp/opencode/media-val-venv` (offline `uv` install of
`controller/requirements.txt` plus `pytest`). `demo/cache` was symlinked to the
existing Alpine 3.21.3 ISO cache because the worktree lives on `/tmp`.

This was validation only. No product source was changed and nothing was
committed; the live checkout received only this report section.

### Result summary

| Check | Result | Evidence |
|---|---|---|
| Build `ozma-node` at HEAD | **PASS** | offline `cargo build` exit 0 in 45.55 s; `ozma-node 0.1.0` |
| Default `demo/run_demo.sh` (Rust) | **PASS** | two Rust VM nodes + controller started; `Backend: rust (default: rust)` |
| mDNS discovery by unmodified controller | **PASS** | `/api/v1/nodes` returned `vm1._ozma._udp.local.` and `vm2._ozma._udp.local.` with ports 7332/7333 and `api_port` 7382/7383 |
| Node health | **PASS** | `/health` 200 `ok:true` on 7382 and 7383 |
| Local HLS | **PASS** | `stream.m3u8` 200; media sequence 127 → 131 in 4 s; `seg_00130.ts` 100,392 B |
| Controller HLS proxy | **PASS** | `/api/v1/streams/<id>/hls/stream.m3u8` and segment 200 `video/mp2t`, 100,392 B |
| Snapshot, node and controller proxy | **PASS** | node `/display/snapshot` 200 JPEG 1280x800 ~96.5 KB; controller proxy 200 `image/jpeg` 96,554 B |
| QMP input | **PASS** | `/input/type` 200 `{"events":2,"ok":true}` on both nodes |
| Graceful stop / no orphans | **PASS** | `demo stop` exit 0 in 0.014 s; nodes logged `SIGTERM` + `Ozma node shutting down`; controller `Application shutdown complete`; no processes, ports, sockets afterwards |
| `/video` MJPEG stream | **PASS** | 8 JPEG parts, 1280x800, exact multipart content type, 2 distinct frames |
| `/video` fps/quality bounds | **PASS** | `fps=0`→1, `fps=999`→30, `quality=0`→1, `quality=999`→100; `/display/mjpeg` alias identical |
| `/video` invalid query | **PASS** | `fps=abc`, `quality=abc`, `fps=-1`, `fps=1.5` all typed `400 video.invalid_query` |
| `/video` unknown console | **PASS** | `?console=bogus` typed `404 not_found` with `details.available=[]` |
| `/video` concurrency / disconnect | **PASS** | 4 slots active, 5th `503 video.stream_limit`, `active_streams` 4→0 after close, new stream 200 |
| VNC server bind + auth | **PASS** | `--vnc-server --vnc-server-port 0 --vnc-password-file`; bound `127.0.0.1:32987`, `security="vnc-auth"`, geometry 1280x800 |
| VNC frames via password auth | **PASS** | `asyncvnc` correct password: `(800,1280,4)` frames, 16 changed pixels between refreshes |
| VNC input reaches guest | **PASS** | typing `ab`, Return, pointer move/click changed 309 pixels, bbox `(0,626)-(150,651)` |
| VNC wrong password rejected | **PASS** | client `PermissionError: Auth failed`; server `VNC authentication failed` |
| TigerVNC RFB 3.8 + VncAuth | **PASS** | TigerVNC 1.16.2: `Server offers security type VncAuth(2)`, `Authentication success!`, `Initialisation done` |
| TigerVNC ZRLE negotiation/decoding | **PASS** | server `count=27 preferred=Zrle cursor=true`; Xvfb screenshot had 29,494 non-black pixels / 339 colours |
| VNC registration advertises bound port | **PASS** | `/api/v1/connection.registration.vnc` = `{"state":"configured","host":"127.0.0.1","port":32987}`; `ss` showed PID fd on 32987 |
| VNC registration retracts on source stop | **PASS** | after QMP source ended: `{"state":"not-configured"}`; listener closed; `/health` still 200 |
| `demo test` suite | **FAIL (non-media)** | core audio `[14]` link FAIL; extended 70 passed / 5 failed / 0 skipped (F19, F20, F25, F26 x2) |
| `demo test` cleanup on failure | **PASS** | failing suite still printed `Stopping Ozma demo...`, killed controller/nodes/VMs, no leftovers |
| Clean-host `--host` | **PASS** | 22 passed / 0 failed / 0 skipped; `ozma-node --lib` 515 passed / 3 ignored; `ozma-softnode` 139 passed; synthetic `/health` PASS |
| Process/socket/temp cleanup | **PASS** | no run processes, demo listeners, QMP sockets, pidfiles, `/dev/shm/ozma-*`, or Xvfb `:99` state |

### `/video` evidence

The demo node at 7382 served the real QMP screendump source. Request:
`GET /video?fps=10&quality=70`.

```text
200
Content-Type: multipart/x-mixed-replace; boundary=ozma-frame
Cache-Control: no-store
Pragma: no-cache
X-Accel-Buffering: no
X-Ozma-Fps: 10
X-Ozma-Quality: 70
Transfer-Encoding: chunked

part 0: bytes=76435 jpeg=ok size=(1280, 800) sha256=5d37f19acbf77c31
part 2: bytes=76417 jpeg=ok size=(1280, 800) sha256=9f73ce4c6c4de048
...
distinct 2 of 8
```

Every part had a real `Content-Length` and decoded as a JPEG; the two hashes are
the QMP console's changing frames. The alias `/display/mjpeg?fps=5&quality=60`
returned the same content type and headers. No bearer token was supplied, so
the documented public read-only media surface is also confirmed.

Concurrency used four held `fps=1` streams:

```text
before (0, 4)
stream0..3 status 200
while-4 (4, 4)
stream5 status 503
{"code":"video.stream_limit","error":"This node is already serving its limit of 4 MJPEG stream(s).", ...}
while-5 (4, 4)
after-close (0, 4)
recover status 200 active (1, 4)
final (0, 4)
```

### VNC evidence

The isolated node used the real QMP head and started with:

```bash
./target/debug/ozma-node \
  --name media1 --profile vm --qmp-socket /tmp/ozma-vm1.qmp \
  --hid-udp-port 17331 --http-port 17382 \
  --capture-provider qmp --input-provider qmp \
  --vnc-server --vnc-server-port 0 \
  --vnc-password-file /tmp/opencode/vnc-pass.txt \
  --vnc-bind 127.0.0.1 --cap hid,video
```

Server log:

```text
RFB 3.8 VNC server listening bind=127.0.0.1:32987 loopback=true
  security="vnc-auth" encrypted=false encoding="raw,zrle"
VNC capture source published its first frame; ServerInit geometry fixed
  width=1280 height=800
```

`asyncvnc` (which sends only ZLib in `SetEncodings`) negotiated Raw, so it was
the Raw path; TigerVNC below supplied the ZRLE path. It decoded frames and
input:

```text
wrong_password -> PermissionError: Auth failed
shot1 (800, 1280, 4) mean 4.44
shot2 changed_pixels 16
shot3 after input changed_pixels 309 bbox (0,626)-(150,651)
correct_password -> input_sent_ok
```

The server recorded the failed handshake and the Raw preference:

```text
VNC handshake failed client=1 peer=127.0.0.1:39458
  error=VNC authentication failed
VNC client encoding preferences applied client=2 count=1 preferred=Raw cursor=false
```

TigerVNC 1.16.2 under Xvfb connected with VncAuth, completed RFB 3.8
initialisation, and the server selected ZRLE from the real 27-entry list:

```text
CConnection: Server supports RFB protocol version 3.8
CConnection: Server offers security type VncAuth(2)
CConnection: Authentication success!
CConnection: Initialisation done

VNC client encoding preferences applied client=3 count=27
  preferred=Zrle cursor=true
```

The Xvfb root screenshot taken while the viewer was connected contained 29,494
non-black pixels across 339 colours, confirming the viewer decoded and drew
frames rather than showing an empty/black window.

Registration before the source ended:

```json
{"state":"configured","host":"127.0.0.1","port":32987}
```

`ss` showed `LISTEN 127.0.0.1:32987` owned by node PID 3538301. Killing the QMP
VM ended the capture source; the node logged
`VNC capture source failed; the VNC listener is stopping` and reported:

```json
{"state":"not-configured"}
```

The listener socket was gone and `/health` still returned 200.

### `demo test` and cleanup

`bash demo/run_demo.sh test` exited 1. The core switching/audio suite had one
failure:

```text
[14] Audio state correct after rapid switching
  FAIL  ozma-vm1 monitor linked after rapid switching
```

The extended suite ended `70 passed, 5 failed, 0 skipped`:

```text
[F19] GET /api/v1/macros returns data          FAIL
      (macro recording start/stop both PASS)
[F20] automation script accepted               FAIL
[F25] AI setups used: 0                        FAIL
[F26] noise_cancellation blocked on free tier  FAIL
[F26] reason mentions upgrade:                 FAIL
```

These are the same controller-only Connect assertions recorded earlier plus
the known macro/automation environment failures; no node, `/video`, VNC, HLS,
or switching assertion failed. The audio link failure is a controller/PipeWire
routing check, outside the media surfaces under test.

Cleanup still ran on the failing suite: `Stopping Ozma demo...`, controller and
both node PIDs killed, both QEMU VMs stopped, and no listeners or processes
remained.

### Clean-host and cleanup

Because the detached worktree is on `/tmp`, clean-host used
`OZMA_CLEAN_HOST_TARGET=/home/matt/.cache/ozma-clean-host-media`:

```text
PASS  cargo build -p ozma-node -p ozma-softnode -p ozma-multiseat --bins
test result: ok. 515 passed; 0 failed; 3 ignored
PASS  cargo test -p ozma-node --lib
test result: ok. 139 passed; 0 failed
PASS  cargo test -p ozma-softnode
PASS  ozma-node desktop profile answered /health on port 35144
22 passed, 0 failed, 0 skipped
RESULT: PASS
```

After the runs there were no `ozma-node`, controller, QEMU, HLS ffmpeg, VNC
viewer, or Xvfb processes from this run; no demo listeners on
7380/7332/7333/7382/7383/5901/5922/17382; no `/tmp/ozma-vm*.qmp`, demo
pidfiles, `/dev/shm/ozma-*`, or Xvfb `:99` lock. The detached worktree, build
target, venv, clean-host target, password files, and evidence logs were removed
after this section was written. A pre-existing `/tmp/ozma-clean-host-1000`
directory (13:06, before this run) and pre-existing `/tmp/ozma-webrtc-*`
directories were left untouched. The only matching process at final check was
an unrelated parallel session's `cargo test -p ozma-node --test
desktop_profile` in the main checkout.
