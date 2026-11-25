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
   opened capture source's frame size instead of requiring an exact
   `--webrtc-width/--webrtc-height` match; the resolved geometry is kept for
   later console-switch restarts. `demo/run_demo.sh --webrtc` now passes it, so
   QMP heads no longer fail at the 640x480 default. A two-peer live re-run is
   still required.
2. **`demo test` cleanup.** The test subcommand now records a non-zero result
   without tripping `set -e` and always calls `_stop_processes`, including when
   the stack fails to start, so a failing suite no longer leaves nodes, QEMU,
   ffmpeg, and the controller running.

The controller-only Connect test drift and the stale container image
(`libxdo`) are unchanged and unrelated to the Rust node.
