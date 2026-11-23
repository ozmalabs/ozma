# Replacing the Python node stack with Rust

Status: **Phase 0 implemented; Phase 1 core and the Phase 2 VM-provider, HLS,
controller-input, softnode-supervisor, and helper-first WebRTC work implemented.
Live D-Bus/VNC/audio validation, native/live WebRTC validation, the remaining
device/audio provisioning, and Phases 3–5 remain plans.** See
[`plan-execution-status.md`](plan-execution-status.md) and
[`parity-matrix.md`](parity-matrix.md) for the evidence and remaining gaps.
Scope: the Python `softnode/`, `node/`, and `agent/` processes are replaced by the
Rust `ozma-node` runtime plus its crates. The Python **controller** is a separate,
much larger program and is handled as a later workstream with a compatibility
bridge in the meantime.

## 1. Where we actually are

Three different Rust artifacts currently coexist:

| Artifact | State | Decision |
|---|---|---|
| `ozma-node` (+ `ozma-protocol`, `ozma-transport`, `ozma-capture`, `ozma-encode`, `ozma-decode`, `ozma-audio`, `ozma-input`, `ozma-api`, `ozma-auth`, clients) | The new stack. Real capture/encode/audio/transport/session/client/web work | **Target runtime** |
| `ozma-softnode` | Discovery, manager loops, agent provisioning, meeting detection are real; the per-VM node is a stub; it spawns an `ozma-softnode` binary that does not exist | **Keep as hypervisor supervisor**, rewrite the runtime half |
| `ozma-agent` | Legacy desktop-agent skeleton; `capture.rs` is a TODO; uses `ozma-proto` | **Retire**, fold into `ozma-node --profile desktop` |
| `src/` (`src/softnode`, `src/agent`) | Dead prototypes, not in the workspace | **Delete** |

Python footprint being replaced:

| Tree | Lines | Role |
|---|---:|---|
| `softnode/` | ~11.9k | Per-VM QEMU node, desktop node, multi-VM manager, QMP/D-Bus/Looking Glass, USB/media, WebRTC/HLS, Connect/mesh |
| `node/` | ~6.1k | Hardware node (largely superseded by `ozma-node`) |
| `agent/` | ~14.2k | Desktop seat agent, multiseat, Windows/Linux capture/input/audio, tray/installer |
| `controller/` | ~82.7k | Controller/API/UI; **not** in the node cutover |

What the new Rust stack already covers: registration/mDNS, HTTP API, sessions,
QUIC + WebTransport + RTSP, V4L2 + PipeWire capture, VAAPI + OpenH264 encode,
PipeWire/ALSA audio + Opus, diagnostics/metrics, auth (PAM/SSH/pairing),
native client, Rust→wasm browser client, camera/mic uplink, remote-output
prototypes, and a substantial test suite.

What it does **not** cover, and therefore blocks replacement:

1. Host/VM input injection (only the HID gadget sink exists).
2. VM display capture (QMP screendump, QEMU D-Bus framebuffer, VNC, Looking Glass).
3. VM lifecycle provisioning and power (QMP sockets, D-Bus p2p, evdev
   `input-linux`, PulseAudio, VM restart, libvirt power).
4. Per-VM audio routing (PipeWire sink/monitor selection, VBAN, UAC2).
5. Controller compatibility: UDP HID on 7331 is advertised but not served;
   `/stream/*` HLS and `/webrtc/offer` are proxied by the controller but the
   Rust node does not implement them.
6. Multi-VM supervision wired to the real runtime, multi-display, USB/media,
   Connect/mesh, Hyper-V, desktop-node parity.

## 2. Target architecture

```text
controller (Python, unchanged during cutover)
  ├─ mDNS / POST /api/v1/nodes/register
  ├─ UDP HID :7331          ─┐
  ├─ GET /stream/* (HLS)     ├─ compatibility adapter in ozma-node
  └─ POST /webrtc/offer     ─┘
        │
        ▼
ozma-node (one process per node)
  profiles: hardware | vm | desktop
  providers:
    capture  : V4L2 | PipeWire | QMP | QEMU-DBus | VNC | LookingGlass | V4L2-loopback
    input    : HID gadget | uinput/evdev | QMP HID
    audio    : PipeWire default | named sink monitor | ALSA | VBAN | UAC2
    display  : fake | EVDI | VM consoles / multi-display
    power    : libvirt | QMP
  planes:
    native    : QUIC / WebTransport / RTSP sessions (primary)
    compat    : UDP HID + HLS + WebRTC HTTP surface (temporary)

ozma-softnode (supervisor, same crate)
  discovery (Proxmox/libvirt/QMP) → provisioning → per-VM ozma-node config
  multi-VM lifecycle, status, metrics, exclusion, restart, agent provisioning

ozma-agent → deleted; desktop seats are `ozma-node --profile desktop`
```

Principles:

1. **One runtime.** `ozma-node` owns capture, input, audio, display, transport,
   HTTP, sessions, and metrics for hardware, VM, and desktop nodes.
2. **Providers, not forks.** Every Python backend becomes a trait implementation
   behind the existing seams (`CaptureProvider`, `InputSink`, `DisplayProvider`,
   `AudioSource`/sink, `PowerBackend`).
3. **Compatibility is a module, not an architecture.** The controller-facing
   UDP/HLS/WebRTC surface lives in `ozma-node/src/controller_compat.rs` and is
   removable once the controller speaks native sessions.
4. **No flag day.** Python keeps running until the parity gates for a given
   profile pass; both can register with distinct node ids during migration.
5. **Freeze Python.** After Phase 0, Python gets security fixes only.

## 3. Phases, deliverables, gates

Estimates are engineer-weeks for one experienced contributor; parallelize by
provider.

### Phase 0 — stop the drift (1–2 weeks)

- Delete `src/` prototypes; mark `ozma-agent` deprecated in its README/help.
- Make `ozma-softnode` fail fast and honestly: either run `ozma-node` in-process
  or refuse to start with a clear message. Remove the impossible
  `Command::new("ozma-softnode")` path and the unsupported flags.
- Add `ozma-node --profile {hardware,vm,desktop}` and `NodeConfig` fields for
  capture/input/audio/display/power provider selection.
- Write the parity harness skeleton: run one Python and one Rust node against a
  fake controller + fake QMP server and diff registration JSON, HID events,
  HTTP responses, and stream presence.
- Publish the parity matrix in `docs/parity-matrix.md`.

**Gate 0:** workspace builds; no shipped stub can claim to work; the harness
runs in CI.

### Phase 1 — provider seams (3–4 weeks)

- `ozma-input`:
  - `UinputSink` (Linux evdev/uinput keyboard + absolute/relative pointer),
    stateful diffing, `KEY_MAX` handling, udev marker/permissions notes.
  - `QmpSink` (HID usage → QMP qcode, keyboard/mouse state machine, button
    tracking), fallback when uinput/`input-linux` is unavailable.
  - Sink selection chain: `uinput → qmp → hid-gadget → recording`, configurable.
- `ozma-qmp` (new crate):
  - QMP greeting/capabilities, control + input sockets, event loop, `screendump`,
    `input-send-event`, `system_powerdown`/`system_reset`, `add_client` for
    D-Bus, `getfd`, timeouts and reconnects.
  - Port Python tests: `test_qmp_client.py`, `test_hid_to_qmp.py`.
- `ozma-capture` providers:
  - `QmpDisplayProvider` (PPM screendump → frames, fps pacing).
  - `DBusDisplayProvider` (QEMU D-Bus framebuffer updates, BGRA).
  - `VncDisplayProvider` (unix socket → socat → VNC, as a transition path).
  - `VirtualCaptureProvider` (v4l2loopback lifecycle).
  - `LookingGlassProvider` (KVMFR header/frame parsing; optional).
- `ozma-audio`: named PipeWire target/monitor selection, per-sink monitor
  capture, optional VBAN encode; later UAC2 injection.
- `ozma-display`: VM console/multi-display model; keep EVDI path separate.
- `ozma-power`: `LibvirtPower`, `QmpPower`.
- Encoder policy: keep VAAPI/OpenH264; add an encoder-override HTTP surface later.

**Gate 1:** unit tests for every provider; QMP and HID tests ported; no
regression in the hardware profile.

### Phase 2 — VM profile + controller compatibility (4–6 weeks)

- `ozma-node --profile vm`:
  - per-VM config from `ozma-softnode` (QMP paths, display type, audio sink,
    capture source, node name/port/api port, guest OS/GPU flags),
  - QMP/D-Bus capture as a `CaptureSource`, uinput/QMP as `InputSink`,
  - HLS pipeline (wire the existing orphaned `ozma-node/src/capture.rs` or the
    new pipeline) writing `/stream/*`,
  - WebRTC via the existing GStreamer worker as an external helper first;
    native `webrtc-rs`/`str0m` evaluation is a follow-up, not a gate.
- `ozma-node/src/controller_compat.rs`:
  - UDP HID v1 (`0x01` keyboard, `0x02` absolute pointer, `0x05` relative),
  - `/health`, `/metrics`, `/api/v1/connection`, `/input/*`, `/audio/*`,
    `/power/state`, `/power/{action}`, `/codec`,
  - `/stream/{file}`, `/webrtc/offer`, `/webrtc/bitrate`,
  - registration payload fields the controller reads: `stream_port`,
    `api_port`, `capture_device`, `display_outputs`, `audio_type`, `audio_sink`,
    `vm_guest_ip`, `pci_devices`, `machine_class`,
  - heartbeat `/api/v1/nodes/heartbeat`.
  - Auth: bind compat endpoints to loopback by default; a shared token option.
- `ozma-softnode` supervisor:
  - port `_provision_vm` (QMP + display sockets + D-Bus p2p + evdev
    `input-linux` + PulseAudio + VM restart) and libvirt power,
  - start `ozma-node` in-process per VM (no child binary), with restart/backoff,
  - expose `status()`, metrics, exclusions, port allocation.

**Gate 2 (the critical one):** the unmodified Python controller can discover,
activate, input, stream HLS, and negotiate WebRTC with a Rust VM node while
**no Python soft node process is running**. One real QEMU VM with QMP passes the
demo scenario end to end.

### Phase 3 — desktop, multi-VM, breadth (4–6 weeks)

- `ozma-node --profile desktop`: uinput input, screen capture (PipeWire
  portal/ScreenCast and X11 fallback), audio sink/routing, `/snapshot`,
  `/status`, `/audio/*`; then delete `ozma-agent`.
- Multi-VM: `ozma-softnode` supervises all discovered VMs; per-VM ports/API;
  status and metrics aggregation.
- Multi-display: `display_outputs` registration, per-console capture,
  controller display picker.
- GPU passthrough: no virtual display; agent-based capture; Looking Glass
  provider; retry-forever agent provisioning (already in Rust).
- Audio: per-VM sink monitor routing, mic path, VBAN/WebRTC audio.
- Optional breadth, only if required by product: Connect relay/mesh, virtual
  USB gadget, USB mass storage, virtual media (FAT32/NBD), Hyper-V, wallpaper.
- Port remaining Python unit tests; add QEMU integration tests to CI (skip when
  no KVM).

**Gate 3:** parity matrix green for VM and desktop profiles; Python processes
are no longer needed for demos; install script works on a clean Linux host.

### Phase 4 — cutover and deletion (2 weeks)

- Switch `demo/`, installers, systemd units, docs, and tests to Rust binaries.
- Keep one release of overlap with Python available behind a tag; then delete
  `softnode/`, `node/`, `agent/`, `src/`.
- Port `softnode/install/install.sh`: udev rules, AppArmor drop-in, libvirt
  `cgroup_device_acl`.
- Publish packaging: `cargo install`/release artifacts, systemd units,
  `ozma-virtual-node` and `ozma-node` service files.

**Gate 4:** fresh-host install uses no Python; regression suite and demo pass;
rollback is a tagged release.

### Phase 5 — controller (separate program)

The Python controller is ~82k lines. Do not couple the node cutover to it.

- **Short term:** keep the Python controller; the compat adapter keeps it
  working. The native session plane (QUIC/WebTransport/RTSP + Rust clients)
  runs alongside.
- **Decision gate after Phase 3:** choose either
  - port the controller in slices (registry/activation/streams → API → UI), or
  - make the native session plane authoritative and treat the controller as a
    discovery/authorization service only.
- If ported, start with `/api/v1/nodes*`, streams, and activation; the UI and
  scenario engine come later.

## 4. Python → Rust mapping

| Python | Rust destination | Action |
|---|---|---|
| `softnode/soft_node.py` | `ozma-node --profile vm` + `controller_compat` + providers | Rewrite on the new runtime |
| `softnode/virtual_node.py` | `ozma-softnode` supervisor (kept, rewritten runtime half) | Port |
| `softnode/desktop_node.py` | `ozma-node --profile desktop` | Port |
| `softnode/qmp_client.py`, `hid_to_qmp.py` | `ozma-qmp` | Port + tests |
| `softnode/evdev_input.py`, `evdev_create.py`, `evdev_service.py` | `ozma-input` `UinputSink` + provisioning | Port |
| `softnode/qemu_display.py`, `dbus_display.py` | `ozma-capture` QMP/D-Bus providers | Port |
| `softnode/virtual_capture.py`, `looking_glass.py` | `ozma-capture` V4L2-loopback / KVMFR providers | Port |
| `softnode/screen_capture.py` | `ozma-capture` desktop provider | Port |
| `softnode/libvirt_power.py` | `ozma-power` libvirt backend | Port |
| `softnode/gst_webrtc*.py`, `webrtc_*.py` | `controller_compat` WebRTC helper (GStreamer) then native | Bridge, then replace |
| `softnode/connect_client.py`, `mesh_interface.py` | `ozma-mesh` / `ozma-agent` (mesh feature) | Port if product needs it |
| `softnode/prometheus_metrics.py` | `ozma-node` metrics | Extend |
| `softnode/virtual_usb.py`, `usb_mass_storage.py`, `virtual_media.py` | new `ozma-usb` / media workstream | Later, scope-gated |
| `softnode/hyperv_node.py` | `ozma-softnode` Windows backend | Later |
| `softnode/wallpaper.py` | desktop command provider | Later |
| `softnode/meeting_detect.py` | merge into Rust `meeting_detect` (audio streams, browser URLs, controller report) | Extend |
| `node/*` | `ozma-node` hardware profile | Already replaced; finish compat |
| `agent/*` | `ozma-node --profile desktop` | Retire after parity |
| `src/*` | — | Delete |

## 5. Parity gates and tests

Each gate gets an automated matrix row; no row is marked done from code
inspection alone.

- **Registration parity:** same fields/resolution for mDNS and direct POST;
  heartbeat keeps the node online.
- **Input parity:** HID v1 packets produce identical evdev/QMP events; key
  release on disconnect; absolute/relative pointer; buttons/wheel.
- **Capture parity:** QMP/D-Bus/VNC frames reach the encoder at target fps;
  multi-display maintains independent sizes.
- **Stream parity:** controller proxies `/stream/*` and `/webrtc/offer`; HLS
  playlists advance; WebRTC answer is valid.
- **Power parity:** state/action endpoints map to libvirt/QMP; unauthorized
  actions rejected.
- **Audio parity:** per-VM sink monitor selected; decoded audio non-silent;
  mic path (where in scope).
- **VM provisioning:** a fresh libvirt VM gains QMP/D-Bus/evdev/audio, restarts,
  and is managed without manual edits.
- **Native plane:** `cargo test --workspace`, QUIC/WebTransport tests, browser
  capture tests remain green.
- **Regression:** controller pytest suite, demo scenario, clean-host install.

Port the 75 existing Python softnode/agent tests first; they are cheap and catch
behavioral drift. Add fake-QMP/fake-D-Bus fixtures so CI needs no hypervisor;
mark real-QEMU tests `#[ignore]`/opt-in.

## 6. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Controller is 82k lines and changes often | Compat adapter + parity harness; no controller work in Phases 0–4 |
| WebRTC has no Rust implementation yet | Keep the GStreamer worker as a supervised helper; replace behind the same endpoint later |
| QMP/D-Bus behavior differs by QEMU/libvirt version | Version matrix tests; keep VNC/screendump fallbacks |
| Input permissions (udev/AppArmor/cgroup) differ | Port the install script early; test on a clean host |
| Dual maintenance drifts | Python feature freeze after Phase 0; cutover date and deletion criteria |
| Compat endpoints weaken security | Loopback by default, token option, no bypass of session auth |
| Naming confusion (`ozma-softnode` crate vs binary vs supervisor) | Rename the binary to `ozma-hypervisor` (or make `ozma-virtual-node` the only supervisor entry) in Phase 0 |
| Hardware not available in CI | Fake providers + recorded fixtures; real-hardware opt-in jobs |

## 7. Immediate next actions

1. Delete `src/` and deprecate `ozma-agent`; document the three-artifact mess in
   the README.
2. Fix `ozma-softnode` to stop spawning a nonexistent binary; run in-process or
   fail fast.
3. Add `ozma-node --profile` and `controller_compat` with `/health`,
   `/metrics`, `/api/v1/connection`, registration payload parity, and a real
   UDP HID listener (replace the TODO in `ozma-node/src/tasks/udp.rs`).
4. Add `ozma-input::UinputSink` with unit tests and a recording sink for CI.
5. Create `ozma-qmp` with QMP connect, HID→qcode, and the ported Python tests.
6. Add the QMP screendump capture provider.
7. Build the parity harness and make Gate 2 (Python controller + Rust VM node,
   no Python soft node) the first demo milestone.

## 8. Definition of done

- One Rust process per node, with `hardware`, `vm`, and `desktop` profiles.
- No Python soft node, hardware node, or desktop agent in demos, installers, or
  services.
- The Python controller (if still present) discovers, activates, inputs, and
  streams Rust nodes through the compat adapter, or the native session plane is
  authoritative and the controller is reduced to discovery/auth.
- Parity matrix, native test suite, and clean-host install are green.
- `softnode/`, `node/`, `agent/`, and `src/` are deleted.
