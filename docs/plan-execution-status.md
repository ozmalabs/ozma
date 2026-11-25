# Plan execution status

Tracking the implementation of [`node-hermes-parity.md`](node-hermes-parity.md)
and the Rust reconciliation in
[`rust-reconciliation-plan.md`](rust-reconciliation-plan.md). This file records
what is actually merged and tested, not what is planned.

Capability and security policy (aperture) is planned in [`aperture-policy.md`](aperture-policy.md); rollout step 1 (`ozma-aperture`) is implemented, and USB/transport/capture/auth adoption remains planned.

USB video output (UVC basic + raw Ozma vendor mode) is planned in [`usb-video-plan.md`](usb-video-plan.md).

The migration from the Python `softnode/`, `node/`, and `agent/` processes to the Rust stack is planned in [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md). Parity evidence is tracked in [`parity-matrix.md`](parity-matrix.md). Phase 4 cutover assets (clean-host harness, Rust-first units, installer, demo switch) are tracked below and in [`clean-host.md`](clean-host.md); the Python trees are retained for the one-release rollback.

## Rust reconciliation — Phase 0

| Item | Status | Evidence |
|---|---|---|
| Dead `src/` prototypes deleted | Done | `src/agent`, `src/softnode`, `src/main.rs`, and `src/streamdeck.rs` removed; they were outside the workspace and had no references |
| `ozma-agent` frozen and marked deprecated | Done | module doc, `Cargo.toml` description, and `--help` all point to `ozma-node --profile desktop` |
| `ozma-softnode` no longer spawns a nonexistent binary | Done | child-process spawn removed; auto-management fails fast with an actionable error; `--no-auto-manage` is discovery-only |
| `ozma-node --profile {hardware,vm,desktop}` | Done (vm/desktop runtime wired) | `NodeProfile` and `ProviderSelection` plus CLI overrides; VM providers and the desktop runtime (PipeWire capture/audio, input chain, routes) are wired in Phases 1–3; see the Phase 3 table below |
| Controller compatibility skeleton | Done | real UDP HID v1 listener, `/api/v1/connection`, registration payload parity, bearer-guarded compat router; `cargo test -p ozma-node --lib` (136 tests, 2 ignored) |
| Parity harness and matrix | Done | `ozma-node/tests/parity.rs` (16 tests, CI-safe fake controller) and [`parity-matrix.md`](parity-matrix.md); `cargo test --workspace` green |
| `ozma-aperture` rollout step 1 | Done | pure meet/provenance/recourse/receipt crate with policy loader; 9 integration tests plus doctest |

Phase 0 does **not** implement QMP/D-Bus/VNC capture, HLS/WebRTC compat routes,
power, VM provisioning, or real EVDI. Those remain Phase 1–3 work and are marked
`not-implemented`/`not-run` in the parity matrix.

## Rust reconciliation — Phase 1 / Phase 2

| Item | Status | Evidence |
|---|---|---|
| `ozma-qmp` QMP client | Done | dual control/input sockets, greeting/capabilities, request/response timeouts, event skipping, reconnect/backoff, `screendump`, `input-send-event`, `add_client`, `getfd`; 62 tests (1 ignored real-QEMU) |
| HID → QMP translation | Done | full `HID_TO_QCODE`, keyboard/mouse/relative state machines, wheel press/release pairs, button diffs; 32 tests |
| Linux uinput sink | Done (unit) | HID→evdev mapping, absolute/relative modes, buttons/wheel, release-all, selection vocabulary; 70 tests, real-device test ignored |
| QMP input sink | Done | `QmpSink` async writer, modifier diffing, wheel/button translation, release-all, fake-QMP integration; 70 `ozma-input` tests |
| `ozma-power` | Done (unit/fake) | `QmpPower`, `LibvirtPower`, `virsh` argv/status/fallbacks, typed errors; 34 tests (1 ignored real-QEMU) |
| QMP capture provider | Done (polling) | `QmpDisplayProvider` PPM screendump → RGB24, fps pacing, typed errors, fake-QMP integration; opt-in registry |
| Looking Glass provider | Done (fixture + VM-profile wiring) | KVMFR parser/source/provider, RGBA/I420 conversion, temp shared-memory fixture. `--capture-provider looking-glass`/`--looking-glass-shm` register it in the VM capture registry with fps/id/name; absent/foreign SHM advertises no source. No real IVSHMEM/GPU run |
| VirtualCapture provider | Done (fake runner) | side-effect-free sysfs discovery, v4l2loopback/ffmpeg lifecycle, rollback/stop; real test ignored |
| VNC display provider | Partial (ffmpeg transition) | `VncDisplayProvider`/`VncDisplaySource` enumerate without I/O, spawn ffmpeg `-f vnc` lazily, read raw RGB24 frames with monotonic PTS/sequence, bound stderr, and map EOF/short frames to typed errors; fake-child tests; real VNC test ignored (`OZMA_VNC_TEST=1`) |
| D-Bus framebuffer provider | Done (unit/fake) | `DBusDisplayProvider`/`DBusDisplaySource`, a small D-Bus wire codec, QMP `getfd`/`add_client`, SCM_RIGHTS listener, `Scanout`/`Update` BGRA frames, and scripted-transport tests; VM profile selection via `--capture-provider dbus` or `--dbus-console`/`--dbus-width`/`--dbus-height`/`--dbus-fps` with `--qmp-socket`. Real QEMU test ignored (`OZMA_DBUS_TEST_QMP`) |
| VM display model and multi-display registration | Done (model + registration) | `VmConsole`/`VmDisplayTopology`, placements, primary/validation, `Console_N` ids; `ozma-node --display-output` parses repeated console specs into the topology, normalizes the primary, and writes registration/TXT `display_outputs` plus `/api/v1/connection.display_outputs`/`display_capture`; D-Bus honours `--capture-console`, QMP/VNC/Looking Glass report the single-head limitation |
| Audio target selection | Done (model + routing) | `AudioTarget`, `<sink>.monitor` naming, `PipeWireCapture::open_target/open_source/open_named_source/open_sink_monitor`; `AudioRouter`/`PipeWireAudioRouter`/`PulseAudioAudioRouter` create/remove null sinks and named `module-remap-source`s, move sink-inputs/source-outputs, apply sink/source profiles, and play reference tracks through `pactl`/`pw-play`/`paplay` with a fake command runner in tests |
| VM profile wiring | Partial | `--qmp-socket`/`--qmp-input-socket`/`--libvirt-domain`, QMP capture registry, `--vnc-host`/`--vnc-port`/`--vnc-width`/`--vnc-height`/`--vnc-fps`, D-Bus display flags/`--capture-provider dbus`/`--capture-console`, Looking Glass `--capture-provider looking-glass`/`--looking-glass-shm|fps|id|name`, multi-console `--display-output` registration, input sink chain (single-socket QMP fallback), QMP/libvirt power, `--audio-router`/`--audio-track-root`, `/power/state`, `/power/{action}`, `/codec`, `/audio/*`, and connection summaries; `cargo test -p ozma-node` |
| HLS pipeline | Partial | `HlsPipeline` reads RGB24/BGRA `CaptureSource` frames into ffmpeg HLS with software encoders (`libx264`, `libx265`, `libsvtav1`, `libaom-av1`, `mjpeg`), `-hls_flags delete_segments`, and a restartable controller that swaps encoders; `/stream/*` serves the output dir; connection reports `running`/`failed`/`not-configured` |
| Controller input compat | Done (recording/fake in CI) | `/input/key`, `/input/mouse`, `/input/type`, `GET/POST /codec`, `/audio/apply|remove|play|stop`, bearer-guarded; HLS codec overrides restart the real HLS pipeline and report `applied: true` only on success, while WebRTC keeps its encoder; audio routes call a real router when configured; real injection/audio is hardware/QMP/audio-server opt-in |
| WebRTC compat | Done (helper-first) | `WebRtcHelper` supervises an external GStreamer/Python worker over a Unix socket, feeds BGRA frames, and proxies offer/bitrate/candidate. Live-validated with the controller, GStreamer peer, Firefox, and Chromium including Opus audio; STUN/TURN config and browser↔helper trickle are implemented. Native WebRTC and helper→browser trickle remain future work |
| `ozma-softnode` per-VM supervisor | Partial | supervises real `ozma-node --profile vm` subprocesses with deterministic ports, restart/backoff, stop/reap, and QMP input fallback; 20 tests |
| `ozma-softnode` provisioning | Partial (opt-in) | `--provision` injects QMP control/input and D-Bus display chardevs into libvirt XML, defines via stdin, and optionally restarts with a bounded socket wait; `--provision-evdev` creates uinput evdev devices and marker/symlink paths, `--provision-audio` creates/reuses `ozma-<vm>` sinks and injects `PULSE_SINK`/`--audio-sink`; both are non-fatal and released on stop. Idempotent, Proxmox-safe, fake-runner/XML/device/sink tests. Real libvirt/uinput/audio validation remains deferred |

**Gate 2 required end-to-end scenario is satisfied at commit
`f5e08e20b6c7156e7bb5acab7e135d201c493937`**: a frozen-tree run with a real
Alpine guest, the repaired unmodified controller, headless Chromium, HLS,
WebRTC video/audio, guest input, snapshots, power pause/resume/reset/off/force-off,
codec switching, and audio routes all passed with captured evidence. See
[`gate2-final-e2e-report.md`](gate2-final-e2e-report.md). Optional breadth and
the controller pytest/clean-host regression remain in the parity matrix.

## Rust reconciliation — Phase 3: desktop profile

| Item | Status | Evidence |
|---|---|---|
| `ozma-node --profile desktop` runtime | Done | real `UinputSink`-first input chain, PipeWire/X11/portal video capture selection, PipeWire audio capture target, snapshots/status, and desktop input gating; [`desktop-profile.md`](desktop-profile.md) |
| Desktop capture registry | Done | selects exactly one transport: `PipeWireCaptureProvider` with `--desktop-capture-target` (`default`, `name:/object:`, `node:<id>`), `X11CaptureProvider` with `--capture-provider x11 --x11-display/--x11-offset/--x11-width/--x11-height/--x11-fps/--ffmpeg-bin`, or `PortalCaptureProvider` built from the node-run ScreenCast handshake. V4L2 and the fake source are not silently substituted; `cargo test -p ozma-node --lib platform::tests::desktop platform::tests::portal_tests` |
| Desktop input | Done (uinput opt-in) | chain is `uinput -> [qmp only if a QMP socket is set] -> hid-gadget -> recording`; a missing `/dev/uinput` leaves the node running with recording-only input and no keyboard/pointer injection capability; `cargo test -p ozma-node --lib platform::tests::desktop_capabilities` |
| Desktop audio | Partial | `--audio-capture-target default|object:|node:|monitor:<sink>` configures session capture; `--audio-router pipewire|pulseaudio` drives `/audio/*` over an existing graph; default capture target is PipeWire then ALSA/fake. `/audio/nodes` still returns the typed 501 graph stub |
| Desktop compatibility routes | Done | public `/status` (alias of `/node/status`) and `/snapshot` (alias of `/display/snapshot`), plus `/display/info`, `/health`, `/api/v1/connection` (including actual `capture` provider/kind/source), `/agent/metrics`, `/ui/hints`; `/audio/*` control routes work when a router is configured and are typed 501 otherwise |
| Desktop startup/route test | Done | `cargo test -p ozma-node --test desktop_profile` starts the real binary with fake capture + recording input, then checks `/health`, `/status`, `/snapshot`, `/display/snapshot`, `/display/info`, `/api/v1/connection`, `/api/v1/capabilities`, and `/audio/*` without PipeWire/uinput/X11 |
| XDG ScreenCast portal consent/selection | Done (fd/restore-token limits) | `--capture-provider portal` runs `PortalScreenCast::connect`/`request_stream` in `main` under `--portal-timeout-ms`, then registers `PortalCaptureProvider`. Denial/timeout/missing portal logs a warning and leaves no capture (no X11/camera/fake fallback). The portal fd/session is held; compositors that allow only the fd fail with a typed unsupported/PipeWire error, and returned restore tokens are not persisted; [`desktop-profile.md`](desktop-profile.md) |
| X11 capture fallback | Done | `--capture-provider x11` registers the real ffmpeg `x11grab` provider with an explicit `--x11-display` (never ambient `DISPLAY`), region/mode/fps flags, typed missing-display/ffmpeg failures; `cargo test -p ozma-node --lib platform::tests::desktop_x11` and `cargo test -p ozma-capture x11::` |
| Delete `ozma-agent` | Blocked on parity | kept deprecated; remaining items are listed in [`ozma-agent/README.md`](../ozma-agent/README.md) |

Desktop live smoke on this host (2026-11-24): the host has a PipeWire session
and a writable `/dev/uinput`. `XDG_SESSION_TYPE=tty` and `DISPLAY` are empty,
but local X sockets `:0`/`:1`/`:2` exist and are usable with
`XAUTHORITY=/run/user/1000/xauth_zBixvF`. With
`--capture-provider x11 --x11-display :1 --x11-width 320 --x11-height 240
--x11-fps 5 --input-provider fake-recording`, the real binary started, logged
the X11 selection, served `/api/v1/connection` with
`capture.provider=x11`, `kind=x11`, `source=x11`, advertised `encode` bounds
fitted to 320x240@5, and returned a real 320x240 JPEG from `/snapshot`. No
desktop input was injected. With `--capture-provider portal` and a bogus
`DBUS_SESSION_BUS_ADDRESS`, the node logged the portal connect warning, stayed
up with `capture.available=false`, no capture source and **no encode/display**
capability, and `/snapshot` returned a typed 503. A successful portal
consent/selection run was not attempted because it would open an interactive
desktop consent dialog. The earlier default PipeWire provider advertised the
host's real camera node (not a screen), and `/snapshot` returned a typed 503
because that camera negotiates `yuyv`, which the JPEG snapshot path does not
convert. `/audio/nodes` remained a typed 501. See
[`desktop-profile.md`](desktop-profile.md) for the exact limitations.

## Rust reconciliation — Phase 3: multi-display and GPU passthrough

| Item | Status | Evidence |
|---|---|---|
| Multi-display registration/config | Done (VM consoles) | `--display-output <label:WxH[:primary][:source]>` (repeatable, indexed form accepted) parses into `VmDisplayTopology`, validates duplicate indices/zero geometry/multiple primaries, normalizes the primary, and writes registration/TXT `display_outputs` plus `/api/v1/connection.display_outputs`. `--display-outputs-json` remains the raw host-output escape hatch and conflicts with `--display-output`. An implicit single console is derived from QMP/D-Bus/VNC/Looking Glass when no spec is given. `cargo test -p ozma-node --lib display_outputs:: config::tests::display_output` |
| Per-console capture selection | Partial (D-Bus runtime switch) | D-Bus honours `--capture-console N`/`--dbus-console` and now also `POST /display/select` (`{"index":N}` or `{"id":"<label>"}`), which validates against `display_outputs`, rebuilds the capture registry for that `Console_N`, restarts running HLS/WebRTC from the shared registry, and updates `/api/v1/connection.display_outputs`/`display_capture`. Selecting the active console is idempotent; a failed pipeline restart restores the previous registry/pipelines and returns a typed 502. QMP/VNC/Looking Glass remain single-head and answer a typed 409 for a different console. Only the selected console has a non-empty `capture_source_id` in the connection document. **Multi-console snapshot broker:** `GET /display/snapshot?console=<N|label|Console_N|source-id>` and `GET /display/info?console=...` use a bounded per-console broker (`ozma-node/src/display_broker.rs`) independent of the active HLS/WebRTC registry. The active console (default/no query) keeps the existing one-shot shared-registry path; D-Bus consoles open separate cached sources (max 4, LRU eviction, 30 s idle eviction, shutdown/select/remove cleanup). QMP/VNC/Looking Glass reject a non-active console with a typed 409, invalid consoles are 404, and no frame is fabricated. The controller forwards `?console=` through its snapshot proxy, exposes `/api/v1/nodes/{id}/display/info?console=...`, and includes the picker value in the dashboard preview. `cargo test -p ozma-node --lib display_broker:: display_control:: config::tests::display_output controller_compat::tests::display_select controller_compat::tests::display_snapshot_and_info_accept_a_per_console_query hls::tests::controller_console_switch_generation_appends_the_existing_playlist`; live QEMU 11.1.3 two-head D-Bus run (`virtio-vga` + `VGA`, Alpine 3.21) switched Console_0 (1280x800 login/shell) ↔ Console_1 (720x400 second-VGA boot text) with real HLS/WebRTC frame-content changes, non-decreasing HLS media sequence plus `#EXT-X-DISCONTINUITY`, and a forced WebRTC failure returned 502 `rolled_back:true` while restoring Console_0; a follow-up live run captured Console_0 (1280x800) and Console_1 (720x400) JPEGs simultaneously while the active console stayed 0. **Per-console HLS:** `--multi-console-hls` (`--hls-per-console`, `OZMA_MULTI_CONSOLE_HLS`) runs bounded independent `HlsController`/ffmpeg generations for every advertised D-Bus console (`--multi-console-hls-max`, default 2 including the active console; `--multi-console-hls-idle-seconds`, default 60, stops non-active pipelines idle without a stream request). Outputs are `<hls-root>/console-<N>/stream.m3u8`, served at `/stream/console-<N>/...` while `/stream/...` follows the active console; `/api/v1/connection.multi_console_hls` and `/display/info` expose per-console status/error/playlist. Startup and `POST /display/select` reconcile add/remove/active state without restarting existing per-console encoders; shutdown stops all with the bounded HLS wait. QMP/VNC/Looking Glass start nothing and report a typed `unsupported` limitation. Live QEMU two-head run: both playlists/segments decoded together (`console-0` h264 1280x800, `console-1` h264 720x400, different segment/frame hashes; OCR showed Alpine package install vs kernel PCI boot text), active `/stream` matched console-0, and SIGTERM exited in ~200 ms. WebRTC is active-console by default; the separate opt-in `--multi-console-webrtc` (`--webrtc-per-console`, `OZMA_MULTI_CONSOLE_WEBRTC`, max default 2, idle default 60 s) runs one external helper per advertised D-Bus console with derived socket/shm paths and geometry, routes `/webrtc/offer` and `/webrtc/candidate` by `console`/`id` or remembered session, keeps a bare offer on the active console, and exposes per-console helper state in `/api/v1/connection.multi_console_webrtc`; single-head providers keep the legacy helper and report a typed `unsupported` limitation. `/codec` fans out to every running per-console HLS pipeline (plus the active single path when present), returns `applied:true` only when all targets restarted with the requested encoder, rolls back earlier restarts on failure, and reports per-pipeline encoders plus `fan_out` state on `GET /codec`; the bound is a pipeline count rather than a measured encoder-capacity model and `/codec` never claims a WebRTC switch. See [`phase3-live-console-switch-report.md`](phase3-live-console-switch-report.md) §14. |
| GPU passthrough / Looking Glass | Done (fixture) | `--capture-provider looking-glass` (or `--looking-glass-shm`) registers `LookingGlassProvider` with `--looking-glass-fps`/`id`/`name`. Missing/foreign SHM or an unpublished frame advertises no/placeholder source, never a hardware claim. `cargo test -p ozma-node --lib platform::tests::vm_profile_registers_looking_glass config::tests::looking_glass` |
| Controller display picker | Done (proxy + dashboard) | `GET /api/v1/nodes/{id}/display` reads the node's `display_outputs`/`display_capture`; `POST /api/v1/nodes/{id}/display/select` forwards `{index|id}` to the node with the per-node/global bearer token, preserves typed 400/404/409/501 limitations, and updates the controller's cached outputs after success. `GET /api/v1/nodes/{id}/display/info?console=...` proxies per-console geometry/source/availability, and `/api/v1/streams/{id}/snapshot?console=...` forwards the selected console to the node. The dashboard stream modal renders a console picker from `display_outputs`, shows the active console/limitation, passes the picker value through snapshot polling, and restarts the local/WebRTC view after a switch. `pytest tests/unit/controller/test_node_api_token_proxy.py tests/unit/controller/test_dashboard_display_picker.py`. |
| Real GPU/VFIO/KVMFR validation | Not run | CI uses a temp KVMFR fixture only; no real QEMU IVSHMEM, VFIO/iommu group, GPU, or `/dev/shm/looking-glass` run. |

## Rust reconciliation — Phase 3: USB/media foundation

| Item | Status | Evidence |
|---|---|---|
| `ozma-usb` gadget/function model | Done (library) | `UsbGadgetConfig`/`UsbFunction` for HID keyboard/mouse/custom, mass storage, UAC2, and FunctionFS vendor; validation covers safe gadget/function names, non-zero VID/PID, strings, power, duplicate links, HID report shape, UAC2 rates/sizes, and CD-ROM read-only. `cargo test -p ozma-usb --test config_validation` (11 passed) |
| `ozma-usb` virtual media | Done (model + overlay) | `MassStorageImage`/`VirtualMedia`, `MediaMode::{Disk,CdRom}`, `MediaState::{Attached,Detached}`, read-only policy, size, and attach validation; `WriteOverlay` adds bounded copy-on-write FAT32/raw blocks, allocation accounting, and checksummed atomic persistence. No mount, image creation, SCSI/NBD overlay wiring, or block I/O. `cargo test -p ozma-usb --test media_model --test fat32_overlay` |
| ConfigFS gadget controller | Done (temp-root tests) | Safe `std::fs` controller with injectable root, descriptor/string/config/function/link creation, HID/mass-storage/UAC2 attributes, `bind`/`unbind`, status/list, attach/detach LUN `file`/`ro`/`cdrom`, typed missing-root/gadget/function/media errors, and rollback on create failure. `cargo test -p ozma-usb --test configfs_controller` (12 passed) |
| Fake controller and USB/IP fallback | Done (unit/fake runner) | In-memory `FakeUsbGadgetController` mirrors create/bind/status/media; `SubprocessUsbController` builds argv-only `usbip` commands (no shell), validates bus IDs/hosts/ports, parses `usbip list -l`, and injects a `CommandRunner`. `cargo test -p ozma-usb --test fake_controller --test subprocess` (11 passed) |
| FunctionFS lifecycle | Done (temp root/fake mounter) | `FunctionFsLifecycle` creates the gadget/`ffs.<instance>`/config link, validates or mounts the expected mountpoint via an injectable `FunctionFsMounter` (`CommandFunctionFsMounter` argv-only, `FakeFunctionFsMounter`), writes v2 descriptors/strings to `ep0`, polls daemon readiness with a typed timeout, refuses premature bind, rolls back on bind failure, and unbinds/cleans up. `cargo test -p ozma-usb --test functionfs_lifecycle` |
| Capability/status shape | Done (model) | `UsbGadgetCapabilities` (`gadget`, `usbip`, `virtual_media`, `mass_storage`, `configfs`, function kinds), `GadgetStatus`/`FunctionStatus`, and `protocol_capability()` projection onto `ozma_protocol::UsbCapability`. `cargo test -p ozma-usb --test status_shape` (3 passed) |
| Node/controller wiring | Done (opt-in, temp-root/fake tests) | `ozma-node` exposes `--usb-gadget`, `--usb-configfs-root`, `--usb-udc`, `--usb-media`, `--usb-media-ro`, and `--usb-media-cdrom` (and `OZMA_USB_*` envs). It builds a HID keyboard/mouse gadget plus an optional mass-storage LUN, creates and binds it through `ConfigFsUsbController` only when a UDC is supplied and the root exists, and adds `usb` to capabilities/TXT/registration only after the bind succeeds. A missing UDC/root/permission/media or bind error is logged with its typed `kind()` and leaves the node running with no USB capability. `/usb` replaces the old 501 with a read-only gadget/capabilities/media status when configured, keeps a typed 501 without `--usb-gadget`, and answers a typed 503 for a configured-but-failed gadget. `POST /usb/media/attach`/`detach` use `VirtualMedia`/`UsbGadgetController`, require an existing absolute regular file, enforce CD-ROM read-only, and never mount or write the image. `/api/v1/connection.usb` reports status/bound/UDC/functions/error. Opt-in `GET /usb/nbd` and `POST /usb/nbd/start|stop` serve one absolute FAT32/ISO/raw file over loopback NBD, force ISO/FAT32 read-only, default raw to read-only, return the `nbd://` URL/counters, and reject a second start with a typed 409. Routes stay behind the bearer guard. |
| USB node tests | Done (CI-safe) | Config/flag opt-in and media/UDC validation; capability not advertised from a bare `cap` string; fake-gadget `/usb` and `/api/v1/connection.usb` shapes; attach/detach + path/read-only/CD-ROM validation without path leakage; unavailable-gadget 503; temp-ConfigFS root create/bind/status/detach; bearer guard for `/usb`, media, and NBD routes; NBD start/status/stop and path/read-only validation. No real UDC/ConfigFS/mount is touched. |
| Real gadget/media validation | Partial (opt-in only) | One real ConfigFS round trip is `#[ignore]` and gated by `OZMA_USB_CONFIGFS_TEST=1`/`OZMA_USB_UDC`; no dummy_hcd/hardware-UDC run. A real userspace NBD server now serves generated FAT32 and synthetic ISO images and was validated with `qemu-img info`, `qemu-io` read/read-only-write refusal, an opt-in disposable-QEMU QMP USB-storage attachment (`OZMA_NBD_QMP_TEST=1`), and an opt-in disposable-QEMU boot of a synthetic El Torito ISO served read-only over NBD (`OZMA_NBD_BOOT_TEST=1`, guest serial marker + QMP `query-block`). A bounded `WriteOverlay` adds copy-on-write FAT32/raw writes with allocation bounds and checksummed persistence (library-only, not yet in the SCSI/NBD path), and `FunctionFsLifecycle` sequences ConfigFS function/link creation, injectable mount/`ep0` descriptor write, readiness timeout, bind rollback, and cleanup (temp root/fake mounter/fake UDC tests only). Still missing real FunctionFS vendor daemon/UDC events, real UAC2 UDC/FunctionFS isochronous transport plus live ALSA/PipeWire wiring (the `Uac2Bridge` core, packetisation, conversion, counters, and status are testable with injected fakes), live FAT32 synthesis and overlay wiring/FAT repair, ISO directory/Joliet/El Torito parsing, real USB/IP wire security/hotplug, mounts, and node-side gadget teardown. |

The default `ozma-usb` suite is CI-safe: it uses a temporary ConfigFS-shaped
root, the fake controller, recording command runners, in-memory transports, and
ephemeral loopback NBD listeners. It never creates a real gadget, binds a real
UDC, mounts anything, or writes a block device. The QEMU live tests skip when
`qemu-img`/`qemu-io` are absent; the disposable-QEMU QMP test is `#[ignore]` and
gated by `OZMA_NBD_QMP_TEST=1`. The node-side USB tests extend this with a
temp-root create/bind/attach/detach round trip, the fake controller, typed
unavailable-gadget fallbacks, and opt-in loopback NBD start/status/stop; they
also never touch a real UDC, ConfigFS mount, or block device.

## Rust reconciliation — Phase 4: cutover assets

| Item | Status | Evidence |
|---|---|---|
| Clean-host harness | Partial (assets green; tests red) | `scripts/clean-host/run.sh` + `Containerfile` check native deps, build both binaries, run a bounded or full test selection, verify `--help`, shell/unit syntax, and a synthetic `/health` start. The 2026-11-25 container run passed dependency, build, `--help`, shell/unit, installer dry-run, `ozma-softnode` (111 tests), and synthetic `/health` checks. `cargo test -p ozma-node --lib` now passes 301 tests after the display-select/WebRTC status fixes (the earlier 2 failures were in parallel in-progress display/WebRTC work). A same-day `--host` rerun from frozen `d2b83e9` passed 17/17 with `ozma-node --lib` 312 passed/3 ignored and `ozma-softnode` 111 passed. Gate 4 is not claimed. See [`clean-host.md`](clean-host.md) and [`phase4-live-demo-report.md`](phase4-live-demo-report.md). |
| Rust-first systemd units | Done (assets) | `scripts/systemd/ozma-node@.service`, `ozma-node-vm@.service`, `ozma-virtual-node.service`, per-profile env examples, and `verify-units.sh`; `systemd-analyze verify` PASS for all three rendered units (systemd 262). Least-privilege and user-unit notes are in `scripts/systemd/README.md`. |
| Install layout | Done (script; not installed on this host) | `scripts/install-rust.sh` installs `ozma-node`/`ozma-virtual-node`, user or system units, and env examples without sudo or enabling units. `--dry-run` writes nothing; Python trees are untouched and remain the rollback path. |
| Demo Rust switch | Done (live 2026-11-25) | `demo/run_demo.sh --rust`, `OZMA_DEMO_BACKEND=rust`, and `OZMA_DEMO_RUST=1` start `ozma-node --profile vm` per demo VM with QMP capture, HLS, and PipeWire monitor audio; Python remains the default and `--python` rolls back. Live run from frozen `d2b83e9`: both nodes were discovered by the unmodified controller via mDNS without `--register-url`; health, node detail, dashboard cards, per-node HLS/snapshot/QMP input, controller proxies, paste, and switching E2E (24 PASS) passed; SIGTERM exited both nodes in 102 ms with no orphan ffmpeg/helper children. See [`phase4-live-demo-report.md`](phase4-live-demo-report.md). The demo still does not wire `--webrtc-helper` (typed 503; helper path proven separately). |
| Fresh-host Python-free install | Not done | Depends on the checkout's own tests going green and an end-to-end run. `/usr/bin/env python` is still required only by the controller, not by the node units/installer. The whole-stack reading of Gate 4 is a Phase 5 controller concern; see the Gate 4a/4b split in [`phase4-cutover-audit.md`](phase4-cutover-audit.md). |
| Port privileged `softnode/install` assets | Scripted and rootlessly verified; privileged host run outstanding | `scripts/install-system.sh` now owns system user/group creation, state/runtime/config dirs, udev rules (uinput/hidg/V4L2/render/optional gadget), the AppArmor libvirt-qemu drop-in, tmpfiles/modules-load snippets, and a copy-only libvirt `cgroup_device_acl` snippet; it never enables/starts a unit, invokes sudo, or edits `qemu.conf`. `scripts/verify-system-install.sh` renders all profiles into temp roots and passes 54/54 checks (AppArmor syntax is SKIP when `apparmor_parser` is absent). | `scripts/verify-system-install.sh`, `scripts/systemd/verify-units.sh` (3 system + 3 user) | The privileged run on a fresh host, merging the libvirt snippet into `qemu.conf`, USB-gadget ConfigFS ownership, and real udev/AppArmor validation remain operator/root steps. |
| Cutover audit (2026-11-25) | **Phase 4 not complete** | Clean detached `4b3b875`: clean-host `--host` 20/20 PASS (`ozma-node --lib` 390 passed/3 ignored, `ozma-softnode` 111 passed), parity 22 passed, multiseat and USB suites green. Blockers: `ozma-virtual-node` child argv omits HLS/display/guest-IP/WebRTC/USB; WebRTC helper is still `softnode/gst_webrtc_worker.py`; `install.sh` installs deprecated `ozma-agent` and points at `uv pip install ozma-softnode`; no `seat_id`/VNC/audio-VBAN/camera registration; controller-only RPA/backup/UI-windows proxies unresolved; Python trees still referenced by demo/CI/build/Dockerfile/dev/tests. Full matrix, per-tree deletion criteria, rollback tags, and staged plan: [`phase4-cutover-audit.md`](phase4-cutover-audit.md). |
| Supervisor child argv parity (P0) | Done (code + tests) | `build_child_argv` forwards HLS/codec, per-VM display/guest-IP/USB/Looking Glass, WebRTC, VNC capture, and audio routing with opt-in flags, a bounded `--help` probe (warn or `--reject-unsupported-node-flags`), secret redaction, and redacted argv/warnings in status. `cargo test -p ozma-softnode` 125 passed. Remaining: no libvirt power/PCI metadata forwarding, single-VM USB. See [`phase4-supervisor-parity.md`](phase4-supervisor-parity.md). |
| Install/runtime cutover (P0) | Done (dry-run; not installed) | `install.sh` defaults to Rust `ozma-node`/`ozma-virtual-node` with `--dry-run` and a labelled `--legacy-python`; `demo/run_demo.sh` defaults to `--rust`; Docker is controller-only; the GStreamer WebRTC helper moved to `scripts/webrtc/` with a compatibility shim and installer integration. Remaining Python references are enumerated file:line in [`phase4-install-cutover.md`](phase4-install-cutover.md). |
| Registration parity (P0) | Done (honest subset) | `seat_id`, configured-only `vnc_host`/`vnc_port`, and `vm_ip` + `vm_guest_ip`; VBAN/camera/Sunshine omitted and advertised as unsupported; credential-shaped TXT keys rejected. Controller discovery updated. See [`phase4-registration-parity.md`](phase4-registration-parity.md). |
| Controller-only surfaces (P0) | Done (typed 501) | `/rpa/*`, `/backup/*`, `/ui/windows`, `/serial/*`, `/rgb/*`, `/current` return stable `supported:false` bodies and are advertised in `/api/v1/connection`; controller proxies preserve them. Real providers remain Python-only. See [`phase4-controller-surfaces.md`](phase4-controller-surfaces.md). |
| Delete Python node trees | Blocked; criteria published | One-release overlap still required. Do not delete until every checkbox in [`phase4-cutover-audit.md`](phase4-cutover-audit.md) §9 is green on a frozen commit and the rollback tag exists. |
| Live default-Rust cutover (validation) | Done (two regressions fixed) | Default `demo/run_demo.sh` passed discovery, health/detail, dashboard, HLS + proxy, snapshots, QMP input, paste, 24/24 switching, and graceful shutdown with no orphans. `--webrtc-fit-capture` and `demo test` cleanup were fixed afterward; a live WebRTC two-peer re-run is still pending. Clean-host `--host` 20/20 on detached `1cd866d`. See [`phase4-live-cutover-report.md`](phase4-live-cutover-report.md). |
| UAC2 bridge → audio/node | Partial | ALSA/PipeWire adapters + dedicated pump; `/usb` reports `configured/running/stopped` with counters and typed errors, and `running` requires PCM. Real UDC/host-enumeration unproven. See [`phase4-uac2-node-report.md`](phase4-uac2-node-report.md). |
| USB FunctionFS/SCSI/USB-IP | Partial | Typed FunctionFS lifecycle, SCSI BBB overlay-backed writable media with persistence, bounded USB/IP daemon with authorization and fail-closed TLS. No real UDC, wire protocol, or `vhci_hcd`. See [`phase4-usb-completeness-report.md`](phase4-usb-completeness-report.md). |
| Multi-seat input isolation | Partial | `ozma-seat-input` helper (`EVIOCGRAB` + uinput) with typed protocol, validation, rollback, idempotent release, and SIGTERM cleanup; supervisor `--input-isolation helper`. Node/compositor does not consume the uinput devices yet. See [`phase4-multiseat-input-report.md`](phase4-multiseat-input-report.md). |

Verification added by this work:

```bash
scripts/clean-host/run.sh --check-deps
scripts/clean-host/run.sh --container          # 2026-11-25: node tests red in parallel work
scripts/clean-host/run.sh --host --full
scripts/systemd/verify-units.sh                # PASS (3 units)
scripts/install-rust.sh --dry-run --from-target target/debug --prefix /tmp/ozma-install
bash -n demo/run_demo.sh scripts/install-rust.sh scripts/clean-host/*.sh
```

## M0 — Contract and cleanup

| Item | Status | Evidence |
|---|---|---|
| Shared `ozma-protocol` crate | Done | capabilities, sessions, permissions, transports, displays, input, control, diagnostics, errors; 19 tests |
| Node builds without native OpenSSL | Done | `ozma-node` uses `reqwest` + `rustls-tls` |
| Versioned node API | Done | `/api/v1/health`, `/node`, `/capabilities`, `/diagnostics`, `/metrics`, `/sessions`, `/displays`, Hestia capability discovery |
| Fake session reaches `Ready`/`Streaming` | Done | `cargo test -p ozma-node --test m0_acceptance` |
| Fake capture/encoder/transport | Done | `ozma-media`, `ozma-capture`, `ozma-encode`, `ozma-transport` |
| OpenAPI/JSON schema | Done | `docs/openapi.yaml` covers the node API, sessions, displays, transport, diagnostics, and errors |
| Orphaned legacy modules wired or deleted | Partly done | `src/` prototypes deleted; `ozma-node/src/tasks/udp.rs` replaced by `controller_compat`. `ozma-node/src/{capture,serve,hotplug}.rs` and the remaining `src/tasks/*` are still unreferenced and stay with the Phase 2 HLS/task wiring |

## M1 — One real media path

| Item | Status | Evidence / limit |
|---|---|---|
| V4L2 discovery | Done | `/dev` + sysfs; no C deps for discovery |
| V4L2 MMAP capture | Done | real single-plane MMAP worker; tested against a real MJPEG camera (1920x1080) |
| PipeWire video capture | Done | `ozma-pipewire` `PipeWireVideoSource` (BGRA/BGRx/RGBx/I420/NV12/YUYV, explicit MemFd buffer params) and `ozma-capture::PipeWireCaptureProvider`; real camera captured 1920x1080 YUYV through PipeWire with 0 dropped. `CaptureRegistry::system_with_pipewire()` keeps V4L2 registered. Portal ScreenCast node adapter point documented; portal consent flow not run |
| Encoder selection/probe | Done | hardware preference, software fallback, probe records |
| Real H.264 encode | Done | `ozma-encode` OpenH264 software backend: MJPEG/NV12/YUYV/RGB/BGRA → I420 → Annex-B, real probe, keyframes |
| Real-camera end-to-end | Validated | `/dev/video0` 1920×1080 MJPEG → VAAPI High-profile H.264 → QUIC fragments → client reassembled and decoded a 45 KB keyframe to 1920×1080 PPM |
| Native QUIC session | Done | real QUIC + rustls, ALPN `ozma/1`, control stream + datagrams |
| Scoped view/control grants | Done | HMAC session tokens, permission sets, view-only input rejection |
| Workstation consent | Done | consent policy + session state machine; denied consent blocks start |
| HID input sink | Done | gadget writes, 6-key limit, absolute/relative reports, release-all, fake/recording sinks |
| Audio path | Done | `ozma-pipewire` native capture/playback and `ozma-audio` Opus; node prefers PipeWire and falls back to ALSA/fake. `AudioReframer` re-blocks 1024-sample PipeWire buffers into 960-sample Opus frames; real PipeWire capture now encodes without errors. `OZMA_AUDIO_SOURCE=fake` provides a paced tone |
| Video fragmentation/reassembly | Done | protocol fragment header, node splitter, client `VideoAssembler` |
| Client camera/mic uplink | Done (browser + native) | `ChannelKind::Camera` + `CaptureStart/Stop/State` + `KeyframeRequest`; node decodes H.264/Opus into a bounded hub with `/api/v1/sessions/{id}/capture` and a PPM proof endpoint. Chromium/Firefox published 1280x720 camera + mic (node decoded both, PPM valid). Native `ozma-client`/`ozma-play` published a real `/dev/video0` (VAAPI/OpenH264, 1920x1080) and PipeWire microphone (499 packets, 958,080 samples); `--camera`/`--mic` flags |
| Input release on disconnect/control loss | Done | `InputReleased` + `ReleaseAll`; QUIC test checks the sink is cleared |
| Session offer with token | Done | `POST /api/v1/sessions` returns a QUIC offer when configured |
| Client video decode | Done | `ozma-decode`: OpenH264 H.264 decoder, I420/RGBA/PPM output, Opus decode; real 1920×1080 frame decoded and saved as PPM |
| Controller connect flow | Done | `ozma-client connect`: controller discovery/activation → node session create/consent/start → transport certificate → QUIC media probe |
| Client playback | Done | `ozma-playback`: PipeWire audio sink with ALSA fallback, null/PPM/window video sinks, bounded playback engine |
| Live playback CLI | Done | `ozma-play`: decode video/audio and present to PPM, null, or optional `minifb` window |
| Native client | Done | `ozma-client` controller/node clients, connect/probe CLI, fragment reassembly, optional decoded PPM frame; desktop rendering is external |
| Input latency measurement | Done (send → encoded frame) | `control.InputLatency`/ack and `ozma-client probe --input-latency`; decode/display/scanout are not included, so no <30 ms SLO claim |

## M2 — Remote monitor spike

| Item | Status | Evidence / limit |
|---|---|---|
| `VirtualDisplayProvider` + fake provider | Done | `ozma-display`, 44 tests |
| EDID builder | Done | 128-byte EDID 1.4 base block, checksum test |
| Topology + input mapping | Done | scale 100/125/150%, rotation 0/90/180/270 |
| API display create/list/destroy/mode/DPMS/topology | Done | backed by the fake provider; real provider gated |
| Xorg + modesetting + EVDI + libevdi spike | Documented, not run | `docs/remote-display-evdi-spike.md` |
| Real EVDI provider | Scaffold only | `EvdiProvider` detects prerequisites and refuses unsupported |
| Browser WebTransport | Done (Chromium + Firefox) | Node `wtransport` listener with ECDSA/14-day certificate pinning, session offers, and uni-stream resources. Headless Chromium ran the full path: CORS session create/start, hash-pinned WebTransport, Hello/HelloAck, live video, and the Rust/wasm viewer rendered 1920x1080 frames. A flipped certificate hash failed the handshake. Firefox/Chrome/Safari matrix and audio are not yet exercised |
| SSH/Unix fallback, passkeys | Not started | — |

## Remote scene outputs (development, loopback)

| Output | Status | Evidence / limit |
|---|---|---|
| RDP | Direct surface + auth validated | FreeRDP shadow subsystem writes OZFR frames into its own primary surface (no Xvfb/ffplay); TLS-only, token/user auth, viewer limit. Stock `xfreerdp3` verified frames and rejection paths. RDPEGFX/H.264 and scene flattening remain. |
| SPICE | Direct surfaces + auth validated | QXL secondary surfaces for image placements, damage rectangles, TLS-only ticket auth, PCM playback channel. `spicy`/screenshot tests verified movement, removal, damage, auth rejection, and playback channel init. One viewer; no SPICE video codec/A-V sync. |
| Xpra | Video path validated at wire level | Handshake fields, PNG delivery, draw acknowledgement, placement-only updates, codec negotiation, and encoded H.264/VP8/VP9/JPEG frames. Stock Xpra remains unverified; compressed hello records are not accepted. |
| Browser scene producer | Live bridge | WebSocket `/api/v1/browser/scene/ws` ingests browser-painted scenes; bounded resource/commit/video hub, `live.mjs` fixture bridge; browser video is metadata-only. This is a producer path, not the viewer. |
| Web viewer | Done (H.264, Chromium + Firefox) | Native Ozma client compiled to wasm32 (`ozma-web`): WebTransport session, shared `ozma-client-core` framing/`VideoAssembler`, WebCodecs H.264, canvas, Opus/Web Audio, pointer/keyboard (USB HID) input, and camera/mic uplink. Chromium and Firefox both rendered live video, decoded a paced Opus stream, published a 1280x720 fake camera (node decoded 117/78 frames) and 376,320 microphone samples, and measured a non-zero analyser level |
| VNC | Development output | RFB 3.8 framebuffer server with damage updates; viewer input discarded. |
| Google Cast | Development output | Live HLS composed output with local playback validation; physical receiver and controller integration remain open. |

These are **development outputs**, not the native Ozma session protocol. RDP and
SPICE currently receive composed pixels; direct graphics surfaces, viewer
authentication, and Xpra video are being extended in separate workstreams.

## Verification

```bash
cargo test --workspace                                   # 2314 passed, 41 ignored (current)
cargo test -p ozma-node --test parity                    # Python/controller parity baseline (16)
cargo test -p ozma-node --lib                            # node API, compat, session unit tests (136, 2 ignored)
cargo test -p ozma-qmp -p ozma-input -p ozma-power       # QMP, uinput/QMP sinks, power (166)
cargo test -p ozma-capture -p ozma-display -p ozma-audio # VM capture, display model, audio target
cargo test -p ozma-aperture                              # aperture meet/receipts/policy loader
cargo test -p ozma-node --test quic_session              # real QUIC end-to-end
cargo test -p ozma-node --test m0_acceptance
cargo test -p ozma-capture                               # opt-in real camera/v4l2loopback tests are ignored
OZMA_CAPTURE_TEST_DEVICE=/dev/video2 \
  cargo test -p ozma-capture -- --ignored real_v4l2_capture
OZMA_VNC_TEST=1 OZMA_VNC_HOST=127.0.0.1 OZMA_VNC_PORT=5901 \
  cargo test -p ozma-capture -- --ignored real_vnc_ffmpeg_captures_a_frame
OZMA_TEST_CAPTURE_DEVICE=/dev/video0 \
  cargo test -p ozma-client --lib -- --ignored real_camera_publishes_frames
```

## Deliberate limitations

- QMP display capture is a polling `screendump` path: every frame costs a QMP
  round trip plus a PPM write/read, and it captures QEMU's default console only.
  D-Bus capture has a real provider and VM-profile resolver, but the real-QEMU
  test is ignored (`OZMA_DBUS_TEST_QMP`) and has not run here. VNC capture is
  implemented only as an ffmpeg transition path: unauthenticated loopback VNC,
  every frame scaled to the configured size, no VNC audio/input, and no live
  VNC/ffmpeg run in CI.
- The uinput sink is unit-tested but its real `/dev/uinput` test is ignored; the
  host user is not in the `input` group. QMP sinks are tested against a fake
  server, not a real QEMU. QMP has no horizontal wheel, touch, or pen event, and
  those inputs are rejected rather than faked.
- `PowerAction::Stop` over QMP calls QEMU `stop` (pause), not destroy; a
  force-off falls back to ACPI powerdown. The libvirt backend uses `virsh`, not
  the libvirt API.
- Audio target selection builds `<sink>.monitor` and named-source targets, and
  the `AudioRouter` can create/remove a named null sink, create/reuse a named
  `module-remap-source` over an existing source/monitor, move sink-inputs and
  source-outputs, apply sink/source volume and mute, and play a reference track
  through `pactl`/`pw-play`/`paplay` when `--audio-router pipewire|pulseaudio`
  is set. `/audio/apply` accepts a named `source` (source-only profiles need no
  sink). It does not implement DSP/room correction, per-VM source/mic
  provisioning, VBAN/UAC2 mic transport, or a live audio-server validation;
  default tests use a fake router. Capturing a mic/source uses the existing
  PipeWire `StreamTarget::Object` path, so the source must already exist in the
  graph.
- HLS is a local rawvideo→ffmpeg pipeline and does not delete old segments.
  WebRTC is helper-first only: it supervises the external GStreamer/Python
  worker, has no native stack or audio, and the current worker does not
  acknowledge bitrate changes. The helper/controller path has now been
  live-validated end to end, including browser video/audio and local STUN/TURN.
  `POST /codec` restarts a running HLS pipeline and reports `applied: true` only
  after a real probe/segment; with HLS off it stores `pending_restart`. `/audio/*`
  calls a real router when configured. Native WebRTC, helper→browser trickle,
  tiled DMABUF import, IME, and real 10-bit HDR capture remain future work.
- `ozma-softnode` supervises `ozma-node --profile vm` subprocesses and allocates
  ports, but it does not create QMP/display sockets, edit libvirt XML, provision
  evdev/PulseAudio devices, or aggregate status/metrics yet.
- Video uses VAAPI when its real probe passes and OpenH264 otherwise.
  NVENC/QSV/V4L2M2M are not implemented; the selection policy refuses to
  pretend a software result is hardware.
- VAAPI forced keyframes restart the FFmpeg sidecar; periodic GOP works
  without a restart.
- QUIC carries control/input/video/audio over a real connection. A second
  WebTransport (HTTP/3) listener carries the same framing for browsers; it is
  covered by a native `wtransport` proof client and by a real headless Chromium
  run and by the wasm viewer itself: headless Chromium connected through
  WebTransport and rendered live 1920x1080 H.264 frames on a canvas; Firefox
  159 rendered live 320x240 frames and both decoded a paced Opus stream. Wrong
  certificate hashes are rejected. Client camera/microphone uplink is being
  added and verified in Chromium/Firefox with fake devices: the node decoded
  camera H.264 and microphone Opus, returned a valid PPM frame, and the
  analyser reported non-silent audio. The native client can publish its own
  camera and microphone with `--camera`/`--mic`. A/V drift is measured but not
  corrected; Firefox loses whole access units when a camera fragment is
  dropped; USB UVC/UAC2 injection, HEVC/AV1/AAC, and Safari remain open.
- Native microphone capture requires a session created with `audio.mic = true`;
  `ozma-play` cannot add the channel to an existing offer.
- PipeWire video capture is opt-in through `CaptureRegistry::system_with_pipewire()`;
  `system()` stays V4L2-only. Portal ScreenCast selection is an adapter point,
  not an implemented consent flow.
- The node RTSP listener broadcasts one default capture/encode pipeline to all
  RTSP viewers and seeds new viewers with the latest keyframe.
- The virtual-display API is exercised through the fake provider. The EVDI
  provider has not been validated on a real Xorg/modesetting session.
- The HTTP API supports an optional static bearer token (`--api-token` /
  `OZMA_API_TOKEN`). Controller-issued short-lived HTTP credentials and
  passkey pairing are still future work.
- The native client library, headless probe, and optional `ozma-play` window
  are done. The window is basic and feature-gated; full desktop UX remains an
  external client concern.
- Audio is PipeWire-native on Linux with an ALSA fallback; fixed 48 kHz stereo
  S16, no A/V sync, jitter buffer, or resampling yet.
- The real-camera test used the USB webcam on this workstation; hardware
  encoder paths, 4K, HDR, and long-run thermal behavior are not validated.
- The HID gadget is unavailable on this host, so input was validated through
  the recording sink rather than a real USB target. When `/dev/hidg0` and
  `/dev/hidg1` cannot be opened, the node uses the fake sink, reports
  `sink_kind: "fake-recording"` on `/api/v1/connection`, and does **not**
  claim to inject input.
- The controller-compat UDP HID listener, `/api/v1/connection`, HLS
  `/stream/*`, `/input/*`, `/power/*`, `/codec`, `/audio/*`, and helper-first
  `/webrtc/*` are implemented. Codec overrides are stored with
  `pending_restart`; audio routing calls a real router only when
  `--audio-router` is configured and otherwise answers typed 501s.
- `ozma-node --profile vm` wires QMP/D-Bus/VNC capture, input, power, and
  optional audio routing when the flags are present; `--profile desktop` still
  records provider selections and does not start uinput/PipeWire providers
  automatically.
- `ozma-softnode` is discovery/status/agent-provisioning only. It no longer
  spawns a nonexistent child binary; auto-management fails fast and points at
  `ozma-node --profile vm`.
- `ozma-node` still carries unreferenced legacy capture/HLS/task modules from
  the earlier sketch (`capture.rs`, `serve.rs`, `hotplug.rs`, and most of
  `src/tasks/*`); they are not part of the build and remain Phase 2 wiring.
- `v4l` pulls `v4l2-sys-mit`, which needs libclang at build time.
