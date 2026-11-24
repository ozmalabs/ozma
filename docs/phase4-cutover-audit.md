# Phase 4 cutover audit

Status: **Phase 4 is not complete and node-tree deletion is not ready.** This
audit found real cutover assets and a green bounded Rust suite on a clean
commit, but it also found product gaps that would break the packaged
supervisor path, WebRTC, seat registration, and several controller features if
`softnode/`, `node/`, and `agent/` were deleted today.

- Audit date: 2026-11-25 (Australia/Sydney)
- Audit baseline: `4b3b875c861a33ddacf7f641248ec99198dd6f47`
  (`feat(node): add Rust software source inventory`)
- Existing preview tag: annotated `rust-cutover-preview-4db92e1`, dereferences
  to `4db92e1a410947d5d19315e5497c6e82b4e4277b`
  (`feat(phase3): codec fan-out, seat resource execution, writable NBD`)
- Working tree: **not clean and not frozen.** `controller/api.py`, routing
  docs, `ozma-multiseat`, `ozma-node`, and `ozma-usb` were modified by parallel
  workstreams during the audit, and a new commit (`4b3b875`) landed after the
  audit began. All test evidence below is from a clean,
  detached worktree at `4b3b875`, not from the moving checkout.
- Method: read-only audit. No Python file was deleted, no product code was
  changed, and no commit or tag was created. The only writes are this document
  and the status/parity links listed at the end.

## 1. Verdict

What is genuinely ready:

- `ozma-node` has hardware/vm/desktop profiles, QMP/D-Bus/VNC/Looking Glass
  capture, QMP/uinput input, HLS, snapshots, power, audio routing, USB gadget
  and NBD, multi-console HLS, display selection, controller compatibility
  routes, and software inventory.
- `ozma-softnode` (`ozma-virtual-node`) has libvirt/Proxmox/QMP discovery,
  port allocation, restart/backoff, opt-in libvirt/QMP/display/evdev/audio
  provisioning, and in-guest agent provisioning.
- `ozma-multiseat` has a real planner, xrandr/xinput/evdev routing, resource
  ownership, and a subprocess seat supervisor.
- Rust-first systemd units, `scripts/install-rust.sh`,
  `scripts/install-system.sh`, and the clean-host harness are implemented and
  the bounded harness passes on `4b3b875`.
- `demo/run_demo.sh --rust` is a working, live-validated demo path at
  `d2b83e9` (see [`phase4-live-demo-report.md`](phase4-live-demo-report.md)),
  but it is **opt-in**; Python remains the default.

What blocks Phase 4:

1. The current checkout is moving and was dirty; no commit is frozen for a
   cutover decision.
2. The packaged supervisor path (`ozma-virtual-node`) does not pass `--hls`,
   `--display-output`, `--vm-guest-ip`, WebRTC, VNC, or USB flags to its
   per-VM children, so it does not match the demo path.
3. WebRTC still requires the Python helper
   `softnode/gst_webrtc_worker.py`, and the demo does not pass
   `--webrtc-helper`.
4. The top-level `install.sh` installs the deprecated Rust `ozma-agent`
   skeleton and tells users to `uv pip install ozma-softnode`; there is no
   `ozma-node --profile desktop` install path there.
5. The Rust node does not register `seat_id`, `vnc_host`/`vnc_port`,
   `audio_vban_port`/`mic_vban_port`, `camera_streams`, or `frigate_*`.
6. The controller still proxies Python-only node/agent surfaces (`/rpa/*`,
   `/backup/*`, `/ui/windows`, `/serial/*`, `/rgb/*`, `/audio/nodes`,
   `/audio/sweep`) that Rust nodes either do not implement or answer with a
   typed 501.
7. The Python trees are still referenced by the default demo, `demo/record.py`,
   CI/build/release, the Dockerfile, dev scripts, and 24 Python test files.
8. `ozma-multiseat` and `ozma-meeting-detect` are not installed by
   `scripts/install-rust.sh`, and `ozma-multiseat` is not wired into
   `ozma-node` registration.

**No Gate 4 completion is claimed.**

## 2. Evidence run for this audit

Clean, detached worktree at `4b3b875`; no local modifications:

```bash
git worktree add --detach /tmp/opencode/ozma-head-4db92e1 4b3b875
cd /tmp/opencode/ozma-head-4db92e1
OZMA_CLEAN_HOST_TARGET=/home/matt/ozma/ozma-audit-target \
    scripts/clean-host/run.sh --host
CARGO_TARGET_DIR=/home/matt/ozma/ozma-audit-target \
    cargo test -p ozma-node --test parity
CARGO_TARGET_DIR=/home/matt/ozma/ozma-audit-target \
    cargo test -p ozma-multiseat
CARGO_TARGET_DIR=/home/matt/ozma/ozma-audit-target \
    cargo test -p ozma-usb
```

| Check | Result | Evidence |
|---|---|---|
| `scripts/clean-host/run.sh --host` | **PASS** | 20 passed, 0 failed, 0 skipped; `ozma-node --lib` 390 passed / 3 ignored; `ozma-softnode` 111 passed; desktop `/health` smoke; installer dry-run; 6/6 system+user units |
| `cargo test -p ozma-node --test parity` | **PASS** | 22 passed, 0 failed |
| `cargo test -p ozma-multiseat` | **PASS** | all test binaries green (23 + 16 + 12 + 9 + 8 + 6 + 3 + 2; 6 ignored; 0 failed) |
| `cargo test -p ozma-usb` | **PASS** | all test binaries green (13 + 12 + 10 + 9 + 9 + 8 + 7 + 6 + 6 + 5 + 5 + 5 + 4 + 3 + 2; 6 ignored; 0 failed) |
| Dirty-checkout `cargo check -p ozma-node` | **Transient FAIL then PASS** | Briefly failed `E0432: unresolved import crate::uac2_bridge` while parallel work had `ozma-usb/src/uac2.rs` modified before the untracked `ozma-usb/src/uac2_bridge.rs` appeared. It passed after the file appeared. This is why the audit uses the clean worktree. |
| Live `demo/run_demo.sh --rust` | **NOT RERUN** | Existing evidence is at `d2b83e9`; HEAD is 5 commits and ~34.5k inserted lines later. Re-run on the cutover commit before deletion. |
| Privileged fresh-host install | **NOT RUN** | No sudo/global install, per task constraints. Rootless surface was verified by the harness. |

Environment note: the first clean-worktree run wrote its target to `/tmp`
(tmpfs, 16 GB) and failed at link time with `No space left on device` /
`ld terminated with signal 7 [Bus error]`. The rerun with
`OZMA_CLEAN_HOST_TARGET` on `/home` passed. This was an environmental failure,
not a product failure.

## 3. Cutover matrix

Status vocabulary: **Done** = implemented and asserted by a checked-in test
that passed; **Partial** = implemented but missing a required consumer,
provider, live validation, or packaging step; **Gap** = no Rust path.

### 3.1 `softnode/` (~13.3k Python lines)

| Python subsystem | Rust destination | Status | Evidence / remaining gap |
|---|---|---|---|
| `soft_node.py` — per-VM node: mDNS/direct registration, UDP HID, QMP/D-Bus/VNC capture, HLS, WebRTC, power, audio, snapshots, USB, codec | `ozma-node --profile vm` + `controller_compat` | **Partial** | Live `--rust` demo at `d2b83e9` passed mDNS discovery, health, node detail, HLS, snapshots, QMP input, paste, switching (24 PASS), and shutdown (`docs/phase4-live-demo-report.md`). Gaps: no `vnc_host`/`vnc_port` registration, no `/display/ws` or `/input/ws`, no VNC server, no VBAN, no `capture_device` v4l2loopback feed, WebRTC helper is Python, and the demo does not wire WebRTC. |
| `virtual_node.py` — multi-VM supervisor, Proxmox/libvirt/QMP discovery, provisioning, agent install, GPU passthrough, ports | `ozma-softnode` / `ozma-virtual-node` | **Partial** | Discovery (`src/discovery/{libvirt,proxmox,qmp_scan}.rs`), port allocation, restart/backoff, `--provision` QMP/display/evdev/audio, and guest-agent provisioning (`src/agent.rs`) are implemented and tested (111 tests). **Blocker:** `build_node_spec` (`src/supervisor.rs:89-139`) passes only QMP capture/input/power, name, ports, register URL, and optional audio sink. It does not pass `--hls`, `--display-output`, `--vm-guest-ip`, WebRTC, VNC, or USB flags. GPU passthrough/`has_gpu_passthrough` is discovered but not turned into capture/agent argv. |
| `desktop_node.py` — desktop soft node: HID, audio, screen capture, mDNS | `ozma-node --profile desktop` | **Partial** | PipeWire/X11/portal capture, uinput-first input, audio capture/router, `/status`, `/snapshot`, `/display/info`, `/health`, `/api/v1/connection`, metrics are implemented and tested. Gaps: portal fd-connect/restore-token persistence, `/audio/nodes` and `/audio/sweep` 501, `/display/mjpeg` 501, Windows/macOS capture/input. |
| `qmp_client.py`, `hid_to_qmp.py` | `ozma-qmp`, `ozma-input` | **Done** | Unit/fake-QMP and Gate 2 live evidence; HID→qcode state machines, shared QMP client, `screendump`, input events. |
| `evdev_input.py`, `evdev_create.py`, `evdev_service.py` | `ozma-input` `UinputSink` + `ozma-softnode --provision-evdev` | **Partial** | 70 `ozma-input` tests, 1 ignored real-device test; provisioning creates uinput devices and marker/symlink paths. Real `/dev/uinput`/udev validation is opt-in. |
| `qemu_display.py`, `dbus_display.py` | `ozma-capture` QMP/D-Bus providers | **Done** | Unit/fake plus live two-head QEMU D-Bus evidence in the parity/status docs; shared QMP client prevents a second control connection. |
| `virtual_capture.py`, `looking_glass.py` | `ozma-capture` v4l2loopback/KVMFR | **Partial** | VirtualCapture lifecycle and KVMFR parser/provider are tested with fake runners/fixtures. No real v4l2loopback run in CI, and the node does not create/feed a v4l2loopback device or register `capture_device` for the controller’s virtual-capture path. |
| `screen_capture.py` | `ozma-capture` desktop provider | **Done** | PipeWire, X11 (`ffmpeg x11grab`), and portal selection with typed failures; X11 live smoke recorded in status docs. |
| `libvirt_power.py` | `ozma-power` | **Done (unit/fake)** | `QmpPower`, `LibvirtPower`, `virsh` argv/status/fallbacks, typed errors. No live libvirt run in this audit. |
| `gst_webrtc*.py`, `webrtc_*.py` | `controller_compat` WebRTC helper | **Partial** | `WebRtcHelper` supervises an external worker; offer/answer/ICE/trickle/bitrate and Opus audio were live-validated. **The worker is still `softnode/gst_webrtc_worker.py`.** The demo does not pass `--webrtc-helper`, so demo nodes return typed 503. Native WebRTC is absent. |
| `connect_client.py`, `mesh_interface.py` | `ozma-mesh` / node mesh | **Gap** | `ozma-mesh` is a library with keys/tunnel/WireGuard helpers, but no node process wires it. There is no TAP `ozma0`, Connect registration, or remote-access path in `ozma-node`/`ozma-softnode`. |
| `prometheus_metrics.py` | `ozma-node` metrics | **Partial** | `/agent/metrics` exposes flat CPU/RAM/disk/net/uptime; GPU, temperature, fan, and power-draw are explicitly unavailable. |
| `virtual_usb.py`, `usb_mass_storage.py`, `virtual_media.py` | `ozma-usb` + node `/usb`, `/usb/media/*`, `/usb/nbd*` | **Partial** | ConfigFS model/controller, HID/mass-storage/UAC2/FunctionFS models, FAT32/ISO synthesis, read-only/writable NBD, overlay, and USB/IP argv exist and `cargo test -p ozma-usb` is green. Missing: dummy_hcd virtual USB passthrough, a wired FunctionFS mass-storage daemon, live FAT32 synthesis/sync into the SCSI path, node-side USB/IP attach, and the UAC2→ALSA bridge. |
| `hyperv_node.py` | none | **Gap** | No Hyper-V backend in Rust. |
| `wallpaper.py` | none | **Gap** | No wallpaper command provider. |
| `meeting_detect.py` | `ozma-meeting-detect` (`ozma-softnode`) | **Partial** | Detector and Unix-socket IPC binary exist, but they are not installed by `install-rust.sh` and not wired to the controller report path. |
| `install/install.sh`, udev, AppArmor, libvirt snippet | `scripts/install-system.sh` + `scripts/system/*` | **Done (scripted)** | Renders user/groups, udev, AppArmor drop-in, tmpfiles/modules-load, copy-only `cgroup_device_acl`, and units; rootless verification passes. Privileged host run remains operator work. |

### 3.2 `node/` (~6.1k Python lines)

| Python subsystem | Rust destination | Status | Evidence / remaining gap |
|---|---|---|---|
| `node.py` — hardware node: registration, UDP HID, HID gadget, V4L2, HLS, USB, power, RPA, health/usb API | `ozma-node --profile hardware` | **Partial** | Core HID/capture/HLS/USB/health/connection routes are in `ozma-node`; `/usb` is implemented. Gaps: no `/api/v1/rpa/*`, no `/video`, no serial buffer, no sensor/RGB/PD endpoints. |
| `capture.py`, `video_stream.py` | `ozma-capture`, `ozma-node/src/hls.rs` | **Done/Partial** | V4L2 + HLS pipeline with hardware/software encoder probing and `/stream/*`; live hardware capture not rerun in this audit. |
| `usb_hid.py` | `ozma-usb` HID function + `ozma-hid` | **Done (model/opt-in)** | HID gadget creation/bind is opt-in (`--usb-gadget --usb-udc`); no real UDC in CI. |
| `usb_audio.py` | `ozma-usb` UAC2 + `ozma-audio` | **Partial** | UAC2 descriptors/ConfigFS attributes are modeled; no ALSA/isochronous audio bridge. |
| `usb_storage.py` | `ozma-usb` mass storage + NBD | **Partial** | Node mass-storage LUN and NBD export are wired; no directory→FAT32 synthesis/sync in the node, no live gadget validation. |
| `usb_emulation.py` | none | **Gap** | Descriptor cloning/passthrough/emulation modes are not ported. |
| `usb_pd.py` | none | **Gap** | USB Power Delivery/I²C controller support is not ported. |
| `power.py` | `ozma-power` | **Partial** | QMP/libvirt power exists. The Python hardware node also drives GPIO power/reset pins; that GPIO path is not in `ozma-power`. |
| `rpa.py` | none | **Gap** | Controller `/api/v1/nodes/{id}/rpa/*` proxies still target the Python node (`controller/api.py:4069-4123`); Rust returns 404. |
| `current_sensor.py`, `expansion_sensors.py` | none | **Gap** | No INA219/expansion sensor endpoints. |
| `rgb_leds.py` | none | **Gap** | Rust `/rgb/*` is a typed 501 stub (`controller_compat.rs:2565-2573`). |
| `serial_capture.py` | none | **Gap** | Rust `/serial/*` is a typed 501 stub. |
| `phone_endpoint.py` | none | **Gap** | No phone/USB-C dock endpoint. |
| `self_management.py` | none | **Gap** | No OTA/self-management parity. |
| `hw_detect.py` | partial platform detection | **Partial** | Rust detects profile capabilities and host info, but not the full hardware-board matrix. |
| `connect_client.py` | none | **Gap** | Same Connect/mesh gap as softnode. |
| `prometheus_metrics.py` | `ozma-node` metrics | **Partial** | Same flat metrics limitations. |
| `node/usb_hid_rs/`, `node/usb_hid_udp/` standalone crates | — | **Decision** | Not workspace members; superseded by `ozma-hid`/`ozma-usb` if still needed. Delete or archive with `node/`. |

### 3.3 `agent/` (~24.5k Python lines, including ~13.0k `agent/multiseat`)

| Python subsystem | Rust destination | Status | Evidence / remaining gap |
|---|---|---|---|
| `ozma_desktop_agent.py`, `softnode/desktop_node.py` — desktop agent: HID, audio, screen capture, clipboard, geometry, wallpaper, metrics, room correction | `ozma-node --profile desktop` | **Partial** | Core capture/input/audio/routes are done. Missing: clipboard sync, display geometry/resolution, wallpaper, room-correction sweep, backup/storage, Windows/macOS. |
| `ozma_agent.py` — host-side clipboard/geometry/resolution agent | `ozma-clipboard` exists, not wired | **Gap/Partial** | `ozma-clipboard` is a workspace crate but no node/agent process uses it for controller clipboard sync. |
| `screen_capture.py` | `ozma-capture` desktop | **Done** | Same as softnode desktop capture. |
| `hardware_info.py` (+ `hardware_info.rs` prototype) | none | **Gap** | No hardware/chassis inventory endpoint in `ozma-node`. The new `ozma-node/src/software/` is software-source inventory only, not hardware info. |
| `storage_manager.py`, `storage_admin.py`, `backup.py` | none | **Gap** | No `/backup/*` or storage admin surface. Controller backup proxies (`controller/api.py:10621-10693`) would fail. |
| `app_monitor.py` | none | **Gap** | No app monitoring. |
| `chassis_db.py`, `chassis_models/` | none | **Gap** | No chassis database. |
| `tray.py` | none | **Gap** | No tray UI. |
| `ui_hints.py` | `ozma-node` `/ui/hints` | **Partial** | Returns an honest empty hint document; no window/control discovery. |
| `connect_client.py` | none | **Gap** | Connect/mesh still Python-only. |
| `service.py`, `cli.py`, installers, `agent/install.sh`, `agent/installer/*` | `scripts/install-rust.sh`, `scripts/install-system.sh`, units | **Partial** | Rust install assets exist, but tray/Windows/macOS/packaging parity and update hooks are absent. The top-level `install.sh` still installs `ozma-agent`, not `ozma-node --profile desktop`. |
| `multiseat/*` | `ozma-multiseat` | **Partial** | Planner, xrandr/xinput/evdev routing, ownership, per-seat `ozma-node` argv, and a real supervisor with restart/backoff are implemented and tested. Not wired into `ozma-node` registration; no `seat_id`; no Windows backends; real display/audio ownership is opt-in (`--apply-ownership`, `--apply-routing`). |
| `agent/*.rs` prototypes (`hardware_info.rs`, `hid_injector.rs`, `pipewire_audio.rs`, `display_geometry.rs`) | — | **Decision** | Not workspace members; delete or replace with maintained crate code. |

### 3.4 Controller-only and shared features

| Feature | Where it lives | Rust node status | Cutover impact |
|---|---|---|---|
| RDP | `controller/stream_router.py` (`xfreerdp`/`x11grab`) | N/A (controller-side) | Not removed by the node cutover. It is not provided by the Python softnode either. A future Rust controller port must port it. |
| SPICE | `controller/stream_router.py` (`spicy`/`remote-viewer`) | N/A (controller-side) | Same as RDP. |
| VNC | QEMU VNC is the Python softnode capture source and is registered as `vnc_host`/`vnc_port`; controller reads it directly for `read_screen`, OCR, `agent_engine`, `testbench`, and `StreamCapture` fallback (`controller/stream.py:535-584`, `controller/api.py:3097-3158`, `controller/agent_engine.py:382-408`) | Rust can capture VNC with `--capture-provider vnc`, but does **not** register `vnc_host`/`vnc_port` or serve VNC | HLS/snapshot work; controller VNC-only features degrade for Rust nodes. Decide: register VNC endpoints, or make controller use snapshots/OCR proxies, or drop those features. |
| HLS | Rust `ozma-node/src/hls.rs`, `multi_hls.rs`; controller `stream_hls_proxy` | Done | Controller proxy is **loopback-only** (`controller/api.py:3704-3735` uses `127.0.0.1:<api_port>`), so remote-node HLS proxying is not validated; direct `stream_url` works. |
| WebRTC | Rust `controller_compat` + external helper | Partial | Helper is Python; demo not wired; audio path partial. |
| Audio | `ozma-audio`, `AudioRouter`, `/audio/*` | Partial | Basic routing works; `/audio/nodes`, `/audio/sweep`, VBAN, UAC2 bridge, room correction, desktop sink provisioning remain. |
| USB | `ozma-usb`, node `/usb`/NBD | Partial | Model/ConfigFS/NBD good; virtual USB/PD/emulation/FunctionFS media daemon gaps. |
| Mesh/Connect | Python `softnode/connect_client.py`, `mesh_interface.py`, `agent/connect_client.py`; `ozma-mesh` library | Gap | No wired Rust path. Product decision required. |
| Multiseat | Python `agent/multiseat/*`; `ozma-multiseat` | Partial | Controller groups seats by `<machine>-seat-<n>` name convention; Rust does not send `seat_id`. |
| Camera node fields | Controller `NodeInfo.camera_streams`, `frigate_*`; Python node roles | Gap | Rust `build_txt` does not emit `camera_streams`, `frigate_host`, `frigate_port`, or `sunshine_port`. |

## 4. Python-only features still required by the controller/demo

These are the concrete consumers that would break or silently degrade when the
Python trees are deleted. Each needs one of: a Rust implementation, a typed
501/404 stub the controller handles, or an explicit product decision to drop
it.

| Consumer | Controller path | Python provider | Current Rust behavior |
|---|---|---|---|
| RPA | `GET/POST /api/v1/nodes/{id}/rpa/*` (`controller/api.py:4069-4123`) | `node/rpa.py`, `node/node.py:437-445` | 404 (no route) |
| Backup fleet | `/api/v1/backup/nodes/{id}/...` proxies (`controller/api.py:10621-10693`) | `agent/ozma_desktop_agent.py:1167-1183`, `agent/backup.py` | 404 |
| UI windows | `/ui/windows` (`agent/ozma_desktop_agent.py:996`) | Python agent | 404 |
| Audio graph/sweep | `GET /audio/nodes`, `POST /audio/sweep` (`controller/api.py:2531,2583`) | `softnode/desktop_node.py`, `softnode/soft_node.py` | Typed 501 (`controller_compat.rs:2437-2448`) |
| Serial panel | `/serial/*` | `node/serial_capture.py` | Typed 501 (`controller_compat.rs:2572`) |
| RGB/current | `/rgb/*`, `/current` | `node/rgb_leds.py`, `node/current_sensor.py` | Typed 501 (`controller_compat.rs:2565-2571`) |
| VNC screen read/OCR | `controller/api.py:3097-3158`, `agent_engine.py:382-408`, `testbench.py:364-366` | QEMU VNC via `vnc_host`/`vnc_port` | No VNC registration; snapshots work instead |
| USB virtual media/passthrough | `/api/v1/nodes/{id}/usb` proxy | `softnode/virtual_usb.py`, `virtual_media.py` | `/usb` + NBD implemented; no dummy_hcd/passthrough/PD |
| Mesh/Connect | controller mesh/Connect APIs | `softnode/connect_client.py`, `mesh_interface.py` | No wired Rust path |
| Seat identity | `controller/seats.py` grouping | `seat_id` in direct registration | Rust sends no `seat_id`; name convention only |
| Meeting detection | controller meeting report | `softnode/meeting_detect.py` | `ozma-meeting-detect` exists but is not installed/integrated |
| Wallpaper | desktop commands | `softnode/wallpaper.py` | None |
| Hyper-V | VM discovery/supervision | `softnode/hyperv_node.py` | None |

## 5. Runtime references to Python binaries and trees

Exact references found in shipped/runtime/CI paths (excluding docs and
`__pycache__`):

| File | Reference | Impact |
|---|---|---|
| `demo/run_demo.sh:34` | `DEMO_BACKEND="${OZMA_DEMO_BACKEND:-python}"` | Python is still the default demo backend. |
| `demo/run_demo.sh:123` | `"$PYTHON" "$REPO_ROOT/softnode/soft_node.py" ...` | Default demo starts Python softnodes. |
| `demo/run_demo.sh:254` | `"$PYTHON" "$REPO_ROOT/controller/main.py" --virtual-only` | Demo always needs Python for the controller (allowed by the DoD, but not by a literal “no Python” Gate 4). |
| `demo/run_demo.sh:175,180` | `tests/test_e2e_switching.py`, `tests/test_e2e_features.py` under `$PYTHON` | `demo/run_demo.sh test` depends on Python. |
| `demo/record.py:84` | starts `softnode/soft_node.py` | Demo recorder has no Rust backend switch. |
| `install.sh:23,226-227` | `uv pip install ozma-softnode`, `ozma-softnode --name ...` | Controller installer tells users to install the Python softnode. |
| `install.sh:169-173,184-207` | downloads and enables `ozma-agent` AppImage | Installs the deprecated Rust agent skeleton, not `ozma-node --profile desktop`. |
| `docker-compose.yml:10-11` | `uv pip install ozma-softnode` | Container quick-start still points at Python. |
| `Dockerfile:40` | `COPY softnode/ /opt/ozma/softnode/` | Controller image ships the Python softnode tree even though no controller code imports it. |
| `.github/workflows/ci.yml:47` | `ruff check controller/ node/ softnode/ agent/` | CI lint still covers the Python node trees. |
| `.github/workflows/ci.yml:100` | `python -m py_compile controller/*.py node/*.py softnode/*.py` | CI compile check still covers them. |
| `.github/workflows/build.yml:14` | wheels matrix `[controller, softnode, agent]` | Release builds Python wheels for all three. |
| `.github/workflows/build.yml:59-90` | `pyinstaller ... ozma-agent` | Builds Python agent installers for macOS/Windows. |
| `.github/workflows/release.yml:48` | `cargo build --release --bin ozma-agent` | Releases the deprecated Rust `ozma-agent` crate. |
| `scripts/gate2/qemu-preflight.sh:23,330` | default helper script `softnode/gst_webrtc_worker.py` | Gate 2 WebRTC preflight breaks if `softnode/` is deleted. |
| `scripts/gate2/run-gate2.sh:116` | default `OZMA_WEBRTC_HELPER=python3` | Gate 2 helper remains Python. |
| `dev/riscv-node/connect-to-vms.sh:92,198-199` | `pgrep -f soft_node.py`, pauses it for QMP hotplug | Dev workflow depends on Python softnode. |
| `dev/windows-vm/provision.sh:343` | starts `softnode/soft_node.py` | Dev Windows provisioning depends on it. |
| `dev/scripts/build-controller-usb.sh:310-311`, `build-installer-usb.sh:526-527` | `uv pip install ozma-softnode` | Installer media instructs Python softnode install. |
| `softnode/install/install.sh` | udev/AppArmor/libvirt installer | Superseded by `scripts/install-system.sh`, but still present and referenced from docs/READMEs. |
| `agent/install.sh`, `agent/installer/build.sh` | `uv pip install ozma-agent`, PyInstaller | Python agent install/build path. |
| `tests/conftest.py:16` | adds `softnode`, `node`, `agent` to `sys.path` | Test suite imports the Python trees. |
| 24 files under `tests/` | import/start Python softnode/node/agent or multiseat | Must be ported or deleted with the trees. |

Systemd units and the Rust installers are **clean**:

- `scripts/systemd/ozma-node@.service` `ExecStart=@BIN_DIR@/ozma-node --profile %i ...`
- `scripts/systemd/ozma-node-vm@.service` `ExecStart=@BIN_DIR@/ozma-node --profile vm --name %i ...`
- `scripts/systemd/ozma-virtual-node.service` `ExecStart=@BIN_DIR@/ozma-virtual-node --node-bin @BIN_DIR@/ozma-node ...`
- `scripts/install-system.sh` and `scripts/system/*` contain no Python/softnode
  execution; `scripts/verify-system-install.sh:133` actively fails if they do.
- `scripts/install-rust.sh` installs only `ozma-node` and `ozma-virtual-node`
  (`:137-157`), and the three units (`:210`). It does **not** install
  `ozma-multiseat` or `ozma-meeting-detect`.

## 6. Rust CLI / registration gaps the controller expects

| Expected field/route | Controller evidence | Rust evidence | Effect |
|---|---|---|---|
| `seat_id` | `controller/api.py:244`, `controller/state.py:87`, `controller/api.py:1074` | No `seat_id` in `ozma-node/src` or `build_txt` (`ozma-node/src/main.rs:2640-2696`); `ozma-multiseat` only sets `--name <machine>-seat-<n>` and `OZMA_SEAT_ID` (`ozma-multiseat/src/planner.rs:359,624,645`) | Seat grouping falls back to the name convention; explicit seat identity cannot be registered. mDNS discovery does not parse `seat_id` at all (`controller/discovery.py:125-186`). |
| `vnc_host`, `vnc_port` | `controller/discovery.py:125-127`, `controller/api.py:223-224`, `controller/state.py:44-45` | `build_txt` never inserts them; `--vnc-*` select capture only | Controller VNC screen read/OCR/agent vision/testbench fallback cannot attach to a Rust VM node. |
| `audio_vban_port`, `mic_vban_port` | `controller/discovery.py:138-141`, `controller/api.py:231-232` | Not emitted | VBAN audio nodes unsupported. |
| `camera_streams`, `frigate_host`, `frigate_port`, `sunshine_port` | `controller/discovery.py:144-158`, `controller/api.py:236-239` | Not emitted | Camera-node and Sunshine discovery unsupported. |
| `vm_ip` (mDNS) vs `vm_guest_ip` (direct) | `controller/discovery.py:147` reads `vm_ip`; `DirectRegisterRequest` uses `vm_guest_ip` (`controller/api.py:240`) | `build_txt` emits only `vm_guest_ip` (`ozma-node/src/main.rs:2665-2668`) | mDNS-discovered Rust VM nodes lose the guest IP; direct registration carries it. |
| `--hls` in supervisor children | Demo launcher passes `--hls` (`demo/run_demo.sh:119`) | `ozma-softnode/src/supervisor.rs:89-139` omits it | Systemd/`ozma-virtual-node` children advertise `video`/`stream_port` (because `build_txt` keys stream_port on `cap`) but have no HLS pipeline; controller HLS proxy gets 503. |
| `--display-output` in supervisor children | Controller display picker expects `display_outputs`; demo direct node can pass it | Supervisor omits it; always `qmp` capture | No multi-console/display picker for supervisor-managed VMs. |
| WebRTC helper path | Controller `/webrtc/offer` proxy | `--webrtc-helper` required; demo omits it; helper script is Python | Demo/nodes return typed 503; deleting `softnode/` removes the only helper. |
| `/api/v1/rpa/*` | `controller/api.py:4069-4123` | No route | 404 for hardware-node RPA. |
| `/backup/*`, `/ui/windows` | `controller/api.py:10621-10693`; agent routes | No route | 404; backup/UI features lost. |
| `/audio/nodes`, `/audio/sweep` | `controller/api.py:2531,2583` | Typed 501 (`controller_compat.rs:2437-2448`) | Honest absence; feature not available. |
| `/serial/*`, `/rgb/*`, `/current` | controller legacy panels | Typed 501 (`controller_compat.rs:2565-2573`) | Honest absence. |
| `/display/mjpeg` | controller soft-node stream path | Typed 501 (`controller_compat.rs:2552-2556`) | Only used when `hw == "soft"` (`controller/stream.py:557-570`); Rust nodes use `stream_url` instead. |
| Multiseat / meeting binaries | controller/operator expectations | `ozma-multiseat`/`ozma-meeting-detect` not installed by `install-rust.sh` | Packaged Rust host has no seat/meeting entry point. |

## 7. Safe cutover mechanics validation

### 7.1 Demo

`demo/run_demo.sh --rust` covers the **required VM demo** as last live-tested
at `d2b83e9`:

- starts two `ozma-node --profile vm` processes with QMP capture/input, HLS,
  PipeWire monitor audio when `pactl` exists, and the Python controller;
- mDNS discovery without `--register-url`, health, node list/detail, dashboard
  cards, local and controller-proxied HLS, snapshots, QMP input, paste,
  switching E2E (24 PASS), and graceful SIGTERM all passed.

It does **not** cover:

- WebRTC (no `--webrtc-helper`; typed 503; helper proven separately),
- USB gadget/media,
- multi-console/`--display-output`,
- seat_id/multiseat,
- a Rust controller,
- `demo/record.py` (still hard-coded to Python softnodes).

Also, Python remains the default (`DEMO_BACKEND:-python`), so “switch `demo/`
to Rust binaries” is only half done: the Rust path exists, the default does
not use it.

### 7.2 Installers

| Asset | Covers | Does not cover |
|---|---|---|
| `scripts/install-rust.sh` | `ozma-node`, `ozma-virtual-node`, user/system units, env examples, dry-run, prefix/system modes | controller, `ozma-multiseat`, `ozma-meeting-detect`, Python-free whole-stack install |
| `scripts/install-system.sh` | user/group, dirs, udev, AppArmor, libvirt snippet, tmpfiles/modules-load, units; `--dry-run`, `--root`, profiles, `--no-user` | privileged run, `qemu.conf` merge, real UDC/ConfigFS, actual service start |
| `scripts/verify-system-install.sh` | 77 checks host / 75 container, idempotence, modes, profile groups | real host install, AppArmor syntax when parser absent |
| `scripts/systemd/verify-units.sh` | 3 system + 3 user units | multiseat/meeting units (none exist) |
| `install.sh` (top level) | Python controller venv, `ozma-agent` AppImage, user service | `ozma-node`, `ozma-node --profile desktop`, Rust node install |

### 7.3 Systemd

The three checked-in units are Rust-only and pass `systemd-analyze verify`.
The packaged supervisor unit starts `ozma-virtual-node`, whose child argv gap
(section 6) means a systemd deployment is not demo-equivalent yet.

### 7.4 Tests

On clean `4b3b875`:

- clean-host `--host`: 20/20; `ozma-node --lib` 390 passed / 3 ignored;
  `ozma-softnode` 111 passed; synthetic desktop `/health` pass.
- parity: 22 passed.
- multiseat: all green.
- USB: all green.
- controller pytest and full `cargo test --workspace` were not rerun in this
  audit; the last recorded controller pytest was 2496 passed / 7 skipped /
  1 xfailed at `d2b83e9` (`phase4-live-demo-report.md`). Re-run both on the
  cutover commit.

## 8. Blockers

### P0 — must be fixed before any default switch or deletion

1. **Freeze the tree.** The audit baseline moved during the audit
   (`4db92e1` → `4b3b875`) and the working tree was dirty. No cutover
   decision can be made against a moving tree.
2. **Supervisor argv parity.** `ozma-softnode/src/supervisor.rs:89-139` must
   pass the flags the demo and controller need (`--hls`, `--hls-dir`,
   `--display-output`/`--vm-guest-ip` as appropriate, WebRTC, VNC if used,
   USB opt-ins), or the packaged systemd path will not produce a stream.
3. **WebRTC helper ownership.** Move `gst_webrtc_worker.py` out of
   `softnode/` (or replace it with a native Rust helper) and update
   `demo/run_demo.sh`, `scripts/gate2/qemu-preflight.sh`,
   `scripts/gate2/run-gate2.sh`, and docs. Wire it in the demo or explicitly
   accept the typed 503 as a non-goal.
4. **Real install path.** `install.sh` must install `ozma-node` (desktop at
   minimum, node profiles as appropriate) instead of the deprecated
   `ozma-agent`, and must not tell users to `uv pip install ozma-softnode`.
5. **Registration parity decisions.** Add `seat_id` and decide whether to
   register `vnc_host`/`vnc_port`; align `vm_ip`/`vm_guest_ip`; either emit
   `camera_streams`/`frigate_*` or remove those controller consumers.
6. **Controller-only proxies.** Add typed 501/404 stubs for `/rpa/*`,
   `/backup/*`, `/ui/windows`, and any other Python-only node surface, or
   port them, before deleting the Python providers.
7. **Runtime references.** Remove/redirect the demo, CI, build, Dockerfile,
   dev-script, Gate 2, and test references in section 5.
8. **Install multiseat/meeting binaries** if multiseat and meeting detection
   are in scope; wire `seat_id` into registration.

### P1 — scope decisions required

- `node/` hardware features: USB PD/emulation, sensors, RGB, serial, phone,
  GPIO power, self-management.
- `agent/` desktop features: backup, storage, hardware info, app monitor,
  tray, clipboard, geometry/resolution, wallpaper, Windows/macOS.
- Mesh/Connect, Hyper-V, room correction, VBAN, UAC2 audio bridge.
- `ozma-agent` Rust crate and `release.yml`/`install.sh` dependency.
- `softnode-qmp` standalone crate and `node/usb_hid_*` standalone crates.

### P2 — validation/environment

- Fresh-host **privileged** install and a Python-free node install on a real
  host.
- Full `cargo test --workspace` and controller `pytest tests/unit/controller`
  on the frozen cutover commit.
- Live `demo/run_demo.sh --rust` rerun on the frozen commit.
- Remote-node HLS proxy (controller currently fetches `127.0.0.1`) or an
  explicit local-only product statement.

## 9. Deletion criteria

### Common gate (all trees)

Do not delete any tree until **all** are true on a frozen, tagged commit:

- [ ] `cargo test --workspace` passes, including `ozma-node`, `ozma-softnode`,
      `ozma-multiseat`, `ozma-usb`, and the parity suite.
- [ ] `pytest tests/unit/controller -q` passes.
- [ ] `scripts/clean-host/run.sh --host` passes (20/20 or better).
- [ ] `scripts/clean-host/run.sh --container --full` passes on the release
      image (or the failure is documented as environmental).
- [ ] `demo/run_demo.sh` **default** (no flag) uses Rust and passes the live
      demo; `--python` still works.
- [ ] `bash demo/run_demo.sh test` passes with the Rust backend.
- [ ] `scripts/install-rust.sh --dry-run` and a staged install pass; a
      privileged fresh-host install uses no Python node package.
- [ ] `scripts/verify-system-install.sh` and `scripts/systemd/verify-units.sh`
      pass.
- [ ] `git grep` shows no runtime/CI/build/Dockerfile/test reference to the
      deleted tree outside historical docs/changelogs.
- [ ] The one-release overlap has shipped and the rollback tag exists.
- [ ] The parity matrix has no `not-implemented`/`partial` row whose only
      provider is the tree being deleted, unless an explicit product decision
      records the drop.

### `softnode/`

- [ ] WebRTC helper relocated out of `softnode/`, and demo/Gate 2/docs updated;
      or native WebRTC replaces it.
- [ ] `ozma-virtual-node` child argv matches the demo (HLS, display outputs,
      guest IP, WebRTC, VNC/USB as scoped) and the systemd supervisor path is
      live-tested.
- [ ] `virtual_node.py` parity is closed for discovery (Proxmox/libvirt/QMP),
      provisioning (QMP/display/evdev/audio), agent install, GPU passthrough,
      and restart/status.
- [ ] Virtual USB/media/UAC2/FunctionFS/PD work is either ported to
      `ozma-usb` + node routes or explicitly dropped, with controller `/usb`
      behavior documented.
- [ ] Hyper-V, wallpaper, mesh/Connect, meeting detection, VNC registration,
      and v4l2loopback `capture_device` are ported, stubbed, or dropped.
- [ ] `softnode/install/install.sh` is deleted or reduced to a pointer to
      `scripts/install-system.sh`; no docs tell operators to run it.
- [ ] `tests/unit/softnode/*`, `tests/test_virtual_node_agent.py`, the Proxmox
      tests that load Python softnode, and `tests/conftest.py:16` are ported or
      removed.
- [ ] CI `ruff`/`py_compile`, the `build.yml` `softnode` wheel, the Dockerfile
      `COPY softnode/`, `demo/record.py`, dev scripts, and Gate 2 defaults no
      longer reference `softnode/`.

### `node/`

- [ ] `ozma-node --profile hardware` covers `/health`, `/usb`,
      `/api/v1/connection`, `/metrics`, HID gadget, V4L2/HLS, audio, and power.
- [ ] RPA (`/api/v1/rpa/*`), `/video`, serial, RGB/current, PD, emulation,
      sensors, phone endpoint, and self-management are ported, typed-stubbed,
      or explicitly dropped; controller proxies (`controller/api.py:4069-4123`)
      and tests are updated.
- [ ] GPIO power/reset behavior is ported or declared out of scope.
- [ ] `node/usb_hid_rs`, `node/usb_hid_udp`, and `node/README.md` references are
      resolved.
- [ ] CI `ruff`/`py_compile` and tests that load `node/` are updated.

### `agent/`

- [ ] `ozma-node --profile desktop` closes items 1–3 and 9 of
      `ozma-agent/README.md` (portal fd/restore token, X11 live, desktop audio
      sink provisioning, `/status` keys) or the product decision records the
      drop.
- [ ] `/audio/nodes`, `/audio/sweep`, `/ui/windows`, `/backup/*`, clipboard,
      geometry/resolution, wallpaper, room correction, and Windows/macOS are
      ported, typed-stubbed, or dropped; controller proxies/tests updated.
- [ ] `ozma-multiseat` is wired to `ozma-node` (or at least registration
      `seat_id` and packaging), and the systemd/install surface includes it;
      Python `agent/multiseat` tests are ported or removed.
- [ ] `ozma-agent` Rust crate, `release.yml`, `install.sh`, `build.yml`
      PyInstaller jobs, and `agent/install.sh` are retired/replaced.
- [ ] `agent/*.rs` prototypes are deleted or moved into maintained crates.
- [ ] `tests/unit/agent/*`, `tests/multiseat/*`, and `tests/conftest.py:16`
      are ported or removed.

### `src/`

Already deleted at Phase 0; `src/` does not exist in `4b3b875`. The stale row
in `docs/rust-reconciliation-plan.md:26` is historical; no deletion action is
needed. Verify no new `src/` prototype appears.

## 10. Rollback tag strategy

Current state:

- `rust-cutover-preview-4db92e1` exists and is annotated. It dereferences to
  `4db92e1`; its message calls it a “Verified Rust preview” and a rollback
  target. It is **not** the required
  `rust-cutover-<version>-python-rollback` tag, and it is one commit behind the
  audit baseline (`4b3b875`).
- Python is still the default, so the last Python-default tree is the current
  line, not an older release.

Recommended tagging (do not move or reuse tags):

| Tag | Points at | When | Purpose |
|---|---|---|---|
| `rust-cutover-<version>-python-default` | last frozen commit where Python is the default and the full validation matrix is green | before changing the default | exact rollback target |
| `rust-cutover-<version>-rust-preview` | frozen commit where Rust is the default preview | after Stage B validation | preview/rollback for the Rust-default line |
| `rust-cutover-<version>` | release commit where Rust is the default and the overlap release ships | release | cutover record |
| `python-nodes-removed-<version>` | deletion commit | after overlap | audit trail for the irreversible step |

Annotated tag messages must include: commit SHA, date, validator, the exact
test/demo commands and results, and the rollback command. Do not embed a short
SHA in the tag name (the existing name embeds `4db92e1` and will drift as the
tree moves); put the SHA in the annotation.

Rollback procedure:

```bash
git checkout rust-cutover-<version>-python-default
# Python nodes:
bash demo/run_demo.sh --python
# or:
OZMA_DEMO_BACKEND=python bash demo/run_demo.sh
```

The Rust systemd units do not reference Python, so a rollback also requires
reinstalling the prior unit/binary package or checking out the tag’s install
assets; document that per release.

## 11. Staged cutover plan

### Stage 0 — freeze and baseline (no product change)

1. Stop parallel edits; choose one commit.
2. Run: `cargo test --workspace`; `scripts/clean-host/run.sh --host`;
   `scripts/clean-host/run.sh --container --full`;
   `pytest tests/unit/controller -q`; `bash demo/run_demo.sh --rust` and
   `bash demo/run_demo.sh --python`; `scripts/verify-system-install.sh`;
   `scripts/systemd/verify-units.sh`.
3. Create `rust-cutover-<version>-python-default` with the evidence.
4. Record the result in this document and `plan-execution-status.md`.

Exit: one frozen commit with a complete evidence bundle.

### Stage 1 — Python freeze (policy only)

1. Declare `softnode/`, `node/`, `agent/` security-fix-only in their READMEs
   and CI/review policy (no feature work).
2. Keep all runtime references working; no deletion.
3. Add a CI guard that fails if new Python feature files land outside the
   approved list (optional).

Exit: no new Python node features; the rollback line is stable.

### Stage 2 — default Rust demo (still no deletion)

1. Change `demo/run_demo.sh` default to `rust`; keep `--python` and
   `OZMA_DEMO_BACKEND=python`.
2. Update `demo/record.py` to honor the backend or start `ozma-node`.
3. Fix `ozma-virtual-node` child argv (§6) so the packaged path matches the
   demo; add `--hls`, display outputs/guest IP, and scoped WebRTC/USB.
4. Move the WebRTC helper out of `softnode/`; wire it in the demo or record the
   typed 503 as an accepted non-goal.
5. Replace `install.sh`’s `ozma-agent`/`ozma-softnode` instructions with
   `ozma-node` and the Rust install path; decide whether the top-level script
   installs the controller only or the full stack.
6. Add `seat_id` to direct registration (and mDNS if the controller is
   updated) or explicitly document name-convention grouping.

Validation:

```bash
bash demo/run_demo.sh                 # default must be Rust
bash demo/run_demo.sh --python        # rollback still works
bash demo/run_demo.sh test
pytest tests/unit/controller -q
scripts/clean-host/run.sh --host
scripts/install-rust.sh --dry-run --prefix /tmp/ozma-audit-dry
```

Exit: Rust is the default demo; Python remains a working fallback.

### Stage 3 — one-release overlap

1. Publish both node install paths for one release; release notes state the
   default and the rollback tag.
2. Run a privileged fresh-host install (node profiles + desktop) with no Python
   node package; keep the controller Python if that is the decision.
3. Run the full regression matrix on the release commit:
   `cargo test --workspace`, controller pytest, clean-host host+container,
   live demo, switching E2E, HLS/snapshot/input, WebRTC (if in scope),
   USB/media (if in scope), multiseat (if in scope).
4. Create `rust-cutover-<version>`.

Exit: one release shipped with Rust default and Python available.

### Stage 4 — deletion

1. Delete `softnode/`, `node/`, `agent/` (and any new `src/` prototype).
2. Remove the runtime/CI/build/Dockerfile/dev/test references in §5.
3. Port or delete the 24 Python tests and update `tests/conftest.py`.
4. Remove the `softnode`/`agent`/`node` wheel/installer jobs; retire
   `ozma-agent` and its release jobs, or re-point them to `ozma-node desktop`.
5. Update docs: `rust-reconciliation-plan.md` DoD, `parity-matrix.md`,
   `plan-execution-status.md`, `clean-host.md`, READMEs, and this audit.
6. Run the full matrix again; create `python-nodes-removed-<version>`.

Exit: no Python node/agent process, installer, service, CI job, or test remains;
rollback is the tagged release.

## 12. Controller cutover decision

**Recommendation: keep the Python controller for the node cutover (compat
bridge), do not port it in Phases 0–4.**

Rationale:

- The controller is now ~107.7k Python lines (`controller/*.py`), larger than
  the 82.7k estimate. Porting it would dwarf the node cutover and couple the
  deletion to a rewrite.
- The compat surface is real and live-tested: discovery/registration,
  `/health`, `/api/v1/connection`, `/input/*`, `/audio/*`, `/power/*`, `/codec`,
  HLS, snapshots, WebRTC proxy, USB, and display selection.
- The Definition of Done allows the Python controller to remain if it
  discovers/activates/inputs/streams Rust nodes through the compat adapter.
- The literal Gate 4 phrase “fresh-host install uses no Python” conflicts with
  this DoD. Split it: **Gate 4a** = Python-free node install; **Gate 4b** =
  whole-stack Python-free install, owned by Phase 5. Do not let 4b block the
  node deletion.

Conditions for the compat decision:

1. Every controller call to a Python-only surface is either implemented in
   Rust or a typed, documented 501/404 the controller handles.
2. `seat_id` and (if VNC features remain) `vnc_host`/`vnc_port` are registered,
   or the controller’s fallbacks are documented.
3. The controller HLS proxy’s loopback assumption is fixed or documented as
   local-only.
4. Controller tests cover the compat routes on the frozen commit.
5. The controller is versioned/pinned with the node release; no silent drift.

Phase 5 decision gate (after Stage 3): either port the controller in slices
(registry/activation/streams → API → UI) or make the native session plane
authoritative and reduce the controller to discovery/authorization. Do not
start the port during the node overlap release.

## 13. Evidence required per stage

| Stage | Required evidence |
|---|---|
| 0 | Frozen commit; `cargo test --workspace`; controller pytest; clean-host host+container; live demo Rust+Python; system install verify; units; rollback tag annotation |
| 1 | README/policy diff; CI guard; no feature commits in Python trees |
| 2 | Default `demo/run_demo.sh` starts Rust; `--python` rollback; `demo test`; supervisor child argv test; WebRTC decision; install.sh diff; seat_id test |
| 3 | Overlap release notes; privileged fresh-host node install (no Python node package); full matrix; live demo; HLS/snapshot/input; WebRTC/USB/multiseat if in scope; `rust-cutover-<version>` tag |
| 4 | `git grep` clean; `cargo test --workspace`; controller pytest; clean-host; live demo; fresh install; `python-nodes-removed-<version>` tag; docs updated |

## 14. Cleanup and artifacts

Audit artifacts:

- Clean worktree: `/tmp/opencode/ozma-head-4db92e1` (removed after the audit).
- Build target for the clean run: `/home/matt/ozma/ozma-audit-target` (removed).
- Clean-host log: `/tmp/ozma-clean-host-1000/logs/host-run.log` (final
  `RESULT: PASS`).
- No Python file, product source, commit, or tag was created or modified by
  this audit. The only repository writes are this document and the status/parity
  links in §15.

## 15. Related documents updated by this audit

- [`plan-execution-status.md`](plan-execution-status.md) — Phase 4 audit row
  and blockers.
- [`parity-matrix.md`](parity-matrix.md) — Phase 4 cutover-audit section.
- [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md) — status header
  points at this audit; Gate 4a/4b split noted.
- [`clean-host.md`](clean-host.md) — rollback/tag criteria point at the audit.
- [`README.md`](README.md) — index entry.
- Existing Phase 4 reports: [`phase4-clean-host-report.md`](phase4-clean-host-report.md),
  [`phase4-live-demo-report.md`](phase4-live-demo-report.md),
  [`phase4-system-install-report.md`](phase4-system-install-report.md).
