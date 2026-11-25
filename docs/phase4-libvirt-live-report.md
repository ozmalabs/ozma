# Phase 4 live libvirt power + QMP capture/input report

Date: 2026-11-25 (Australia/Sydney; UTC timestamps below unless noted)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
HEAD: `28ffd6986c7c09844982d12096c39a4035281567`
(`feat(phase4): VNC Hextile/ZLib, mesh discovery, systemd runtime`)
Binary: `target/debug/ozma-node` (411,887,216 bytes, mtime 21:27 local)
Validator: Phase 4 libvirt live-validation subagent

## Verdict

The real libvirt power + QMP capture/input path works end to end against a
disposable domain. Both `qemu:///session` and `qemu:///system` were exercised.
No controller was registered and no host disk/GPU/USB/audio device was passed
through.

| Check | Result | Evidence |
|---|---|---|
| libvirtd / virsh available | **PASS** | libvirtd 12.9.0 `active` + `enabled` before and after; `virsh` 12.9.0 |
| Default network untouched | **PASS** | `default` `active/yes/yes` before and after; no interface attached to the test domain |
| Disposable domain definition/start | **PASS** | session and system domains, Alpine 3.21.3 ISO, no disk, no host passthrough |
| libvirt power backend connects | **PASS** | `libvirt power: connected ... state=running` |
| `GET /power/state` | **PASS** | 200 `{"available":true,"status":"running"}` matching `virsh domstate` |
| `POST /power/pause` | **PASS** | 200; `virsh domstate` = `paused`; node = `paused` |
| `POST /power/resume` | **PASS** | 200; `virsh domstate` = `running`; node = `running` |
| `POST /power/stop` | **PASS** | 200 `force-off`; `virsh domstate` = `shut off`; node = `shutoff`; QMP socket gone |
| `POST /power/start` | **PASS** | 200 `on`; `virsh domstate` = `running`; new QMP socket; node reconnected |
| `--libvirt-uri` / `OZMA_LIBVIRT_URI` | **PASS (fixed)** | every `virsh` call received `--connect qemu:///system`; unset keeps virsh's own default (`qemu:///session` here). See [fixes report](phase4-libvirt-fixes-report.md). |
| `POST /power/off` ACPI honesty | **PASS (fixed)** | still-running guest → HTTP 202 `{"pending":true,"reason":"acpi-requested","status":"running"}`, matching `virsh domstate`; never reported `shutoff` |
| `GET /display/snapshot` (QMP) | **PASS** | 200 `image/jpeg`, 1280x800 and 720x400 frames, real JPEG bodies |
| `POST /input/key` (QMP) | **PASS** | 200 `{"ok":true,"sink":"qmp"}`; frame pixels changed at the guest login prompt; OCR read the typed character |
| Invalid domain error path | **PASS (fixed)** | typed `domain-not-found`; `/power/state` = 503 `{"status":"unavailable"}`, actions 503 with the same code and reason; no fake success and no silent `not-configured` |
| Graceful node shutdown | **PASS** | system node SIGTERM → `shutdown requested` + `Ozma node shutting down`, exited in 53 ms, domain stayed running |
| Cleanup | **PASS** | domains undefined, logs/temp/processes/sockets/ports removed; existing domains and `libvirtd` unchanged |

One deployment gap remains open: a **system-domain QMP socket is created 0775
owned by `libvirt-qemu`**, so an unprivileged node cannot open it without a
permission adjustment. The missing libvirt URI flag and the invalid-domain,
ACPI-result, and pre-first-frame geometry gaps were fixed on 2026-11-25; see
[`phase4-libvirt-fixes-report.md`](phase4-libvirt-fixes-report.md) and
[Remaining gaps](#remaining-gaps).

> **Scope limits.** No real desktop, GPU, USB or audio interaction; no host
> storage passthrough. No controller was started and the node had no
> `--register-url` (mDNS advertising only). VNC capture was not needed or
> tested; QMP `screendump` supplied capture. The test domains used the cached
> Alpine live ISO read-only, with no writable disk and no network interface.

## 1. Preflight

```text
$ which virsh libvirtd qemu-system-x86_64 qemu-img
/usr/bin/virsh
/usr/bin/libvirtd
/usr/bin/qemu-system-x86_64
/usr/bin/qemu-img

$ virsh --version
12.9.0
$ qemu-system-x86_64 --version | head -1
QEMU emulator version 11.1.3

$ systemctl is-active libvirtd
active
$ systemctl is-enabled libvirtd
enabled
```

libvirtd was already running, so it was **not started or stopped** by this test.

The default libvirt connection for the invoking user is session, not system:

```text
$ virsh uri
qemu:///session

$ virsh -c qemu:///system uri
qemu:///system
```

Network and existing domains were recorded before the test and left alone:

```text
$ virsh -c qemu:///system net-list --all
 Name      State    Autostart   Persistent
 default   active   yes         yes

$ virsh -c qemu:///system list --all
 -  reviewpay-autoinstall-test      shut off
 -  reviewpay-autoinstall-test-v7   shut off
 -  reviewpay-autoinstall-test-v8   shut off
 -  winxp                           shut off
 -  winxp-install                   shut off

$ virsh -c qemu:///session list --all
 (empty)
```

No `ozma-node`, `qemu-system-*`, `/tmp/ozma-*.qmp` socket, or test port was
running before the test.

## 2. Disposable domains

Both domains used the same shape:

- 256 MiB RAM, 1 vCPU, `pc` machine, VGA, USB keyboard + USB tablet.
- Read-only CD-ROM only: `demo/cache/alpine-virt-3.21.3-x86_64.iso`.
- **No writable disk, no host block device, no PCI/USB passthrough, no network
  interface.**
- An additional QMP Unix socket supplied through:

```xml
<qemu:commandline xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <qemu:arg value='-qmp'/>
  <qemu:arg value='unix:/tmp/opencode/ozma-libvirt-live/.../qmp.sock,server=on,wait=off'/>
</qemu:commandline>
```

The session domain also logged the guest serial console to a file; the system
domain used a PTY to avoid writing into a root-owned path. The ISO booted to an
Alpine `localhost login:` prompt, which gave a real guest surface for input
verification.

QMP was proven before the node connected (session domain, 10:32Z):

```text
greeting: {"QMP": {"version": {"qemu": {"micro": 3, "minor": 1, "major": 11},
           "package": ""}, "capabilities": ["oob"]}}
qmp_capabilities: {"return": {}}
query-status:    {"return": {"status": "running", "running": true}}
screendump:      {"return": {}}
PPM:             P6 1280 800 255, 3,072,016 bytes
```

QEMU was launched by libvirt with `-accel tcg` (the domain XML used
`type='qemu'`; KVM was not requested). That is intentional for this test and does
not affect the power/capture path.

## 3. Session-domain live run (`qemu:///session`)

### 3.1 Define and start

```bash
TD=/tmp/opencode/ozma-libvirt-live
virsh -c qemu:///session define "$TD/domain.xml"
virsh -c qemu:///session start ozma-phase4-libvirt-live-20261125
virsh -c qemu:///session domstate ozma-phase4-libvirt-live-20261125
# -> running
```

### 3.2 Node command

```bash
cd /home/matt/ozma/ozma
RUST_LOG=info,ozma_qmp=debug,ozma_input=debug ./target/debug/ozma-node \
  --name ozma-phase4-libvirt-live \
  --profile vm \
  --qmp-socket /tmp/opencode/ozma-libvirt-live/qmp.sock \
  --capture-provider qmp \
  --input-provider qmp \
  --power-provider libvirt \
  --libvirt-domain ozma-phase4-libvirt-live-20261125 \
  --http-port 47382 \
  --hid-udp-port 60732 \
  --cap hid,video,power \
  --rtsp-port 0 \
  --webtransport-port 0
```

No `--register-url` was supplied. The node advertised over mDNS but did not
register with a controller.

Selected startup log:

```text
INFO ozma_qmp::client: QMP control connected path=.../qmp.sock
INFO ozma_node: QMP control socket: one shared client for power, capture,
     snapshots, and input owner="node-shared-control"
INFO ozma_node: Input sink ready sink="qmp"
INFO ozma_power::libvirt: libvirt power: connected
     domain=ozma-phase4-libvirt-live-20261125 state=running
INFO ozma_node: libvirt power backend connected
     domain="ozma-phase4-libvirt-live-20261125"
INFO ozma_node::api: Node API listening port=47382
```

### 3.3 State, capture, input

```text
GET /health          -> 200 {"ok":true,...}
GET /power/state     -> 200 {"available":true,"status":"running"}
virsh domstate       -> running

GET /display/info    -> 200 {"active":true,"console":0,"source_id":"qmp0",
                             "snapshot_available":true,
                             "dimensions_source":"capability-max",
                             "width":1920,"height":1080}
GET /display/snapshot -> 200 image/jpeg, 96,488 bytes, JPEG 1280x800
                         sha256 460312dc7df780c5a16e6ab536902abb190fdd1b7c28ab19cffa9e4806c3d12c
```

`/display/info` initially reported the capability maximum (1920x1080). After
the first frame it reported the actual last-frame geometry (1280x800); this is
informational, not a capture failure.

> **Follow-up fix (2026-11-25).** The historical output above is retained as
> evidence. `/display/info` no longer reports `capability-max` as the current
> frame size. Before the first frame it now reports `width:0`, `height:0`,
> `dimensions_source:"unknown"` and exposes the limit separately as
> `capability_max_width:1920`, `capability_max_height:1080`. After a frame it
> reports `last-frame` with the real 1280x800 geometry. Live re-validation and
> exact output are in
> [`phase4-libvirt-fixes-report.md`](phase4-libvirt-fixes-report.md).

Input at the Alpine login prompt (evdev keycode 30 = `a`):

```text
POST /input/key {"keycode":30,"down":true}  -> 200 {"ok":true,"sink":"qmp"}
POST /input/key {"keycode":30,"down":false} -> 200 {"ok":true,"sink":"qmp"}
DEBUG ozma_input::qmp: QMP input events sent on shared client count=1
DEBUG ozma_input::qmp: QMP input events sent on shared client count=1

snapshot before: 1280x800, 96,488 B, sha256 460312dc...6c3d12c
snapshot after:  1280x800, 96,550 B, sha256 2a4cd769...b747369a
pixel diff bbox: (136,624)-(144,640), 53 changed pixels
tesseract crop:  "[in: a"
```

The screenshot proves the key reached the guest: the glyph appeared at the
login prompt and only that 8x16 character cell changed. A second run after a
domain reset sent keycode 44 (`z`); the frame changed in the same cell
(bbox `(136,624)-(152,640)`, 53 pixels) and OCR read `[in: z_`.

### 3.4 Power transitions

| Action | HTTP | Node state | Independent libvirt check |
|---|---|---|---|
| `GET /power/state` (initial) | 200 `running` | `running` | `virsh domstate` = `running` |
| `POST /power/pause` | 200 `{"action":"pause","ok":true}` | `paused` | `virsh domstate` = `paused` |
| `POST /power/resume` | 200 `{"action":"resume","ok":true}` | `running` | `virsh domstate` = `running` |
| `POST /power/stop` | 200 `{"action":"force-off","ok":true}` | `shutoff` | `virsh domstate` = `shut off`; `domid` = `-` |
| `POST /power/start` | 200 `{"action":"on","ok":true}` | `running` | `virsh domstate` = `running`; new libvirt id `2` |

After `stop` the QMP socket was gone and `/display/snapshot` returned a typed
failure rather than a stale frame:

```text
POST /power/stop -> 200 {"action":"force-off","ok":true}
virsh domstate   -> shut off
GET /power/state -> 200 {"available":true,"status":"shutoff"}
QMP socket       -> absent
GET /display/snapshot -> 503
  {"ok":false,"code":"capture.unavailable",
   "error":"No capture source is available for display snapshots.",
   "retryable":false}
```

After `start`, libvirt recreated the QMP socket and the node reconnected its
shared client without a restart:

```text
POST /power/start -> 200 {"action":"on","ok":true}
virsh domstate    -> running
QMP socket        -> present
GET /power/state  -> 200 {"available":true,"status":"running"}
INFO ozma_qmp::client: QMP control connected path=.../qmp.sock   (10:33:57Z)
GET /display/snapshot -> 200 image/jpeg, 20,413 bytes, 720x400
```

Two further semantics were checked live:

```text
POST /power/off   -> 200 {"action":"off","ok":true}
virsh domstate    -> running
GET /power/state  -> 200 {"available":true,"status":"running"}
```

`off`/`shutdown` sends an ACPI `system_powerdown` request (`virsh shutdown`).
The Alpine live ISO has no ACPI handler, so the VM correctly stayed running.
A 200 here means "request delivered", not "guest is off".

```text
POST /power/reset -> 200 {"action":"reset","ok":true}
virsh domstate    -> running
GET /power/state  -> 200 {"available":true,"status":"running"}
```

### 3.5 Supported action mapping

From the live run and the route/backend code:

| HTTP path | Canonical action | libvirt command |
|---|---|---|
| `/power/state` | read | `virsh domstate <domain>` |
| `/power/on`, `/power/start`, `/power/cont` | `on` | `virsh resume`, then `virsh start` on failure |
| `/power/off`, `/power/shutdown`, `/power/powerdown` | `off` | `virsh shutdown` (ACPI request) |
| `/power/pause`, `/power/suspend` | `pause` | `virsh suspend` |
| `/power/resume` | `resume` | `virsh resume` |
| `/power/reset` | `reset` | `virsh reset` |
| `/power/force-off`, `/power/force_off`, `/power/stop`, `/power/destroy` | `force-off` | `virsh destroy` |

Note that `/power/start` is the `on` alias and therefore uses the resume-or-start
sequence; it does not have a separate "start only" endpoint.

An independent second QMP client could not be used while the node was running:
the extra `-qmp` socket is single-client and the node held it. Verification of
power actions therefore used `virsh domstate` (libvirt state), the node's own
`/power/state` (which calls `virsh domstate`), and the node's QMP connection
logs. Input and capture were verified directly from the published frame.

## 4. System-domain live run (`qemu:///system`)

A second disposable domain was defined in the system connection to exercise the
more common deployment shape. This run predated the `--libvirt-uri` flag, so the
environment supplied `LIBVIRT_DEFAULT_URI`; after the fix, pass the URI directly
(it is also accepted as `OZMA_LIBVIRT_URI`):

```bash
./target/debug/ozma-node \
  --name ozma-libvirt-sys \
  --profile vm \
  --qmp-socket /tmp/opencode/ozma-libvirt-live/sys/qmp.sock \
  --capture-provider qmp \
  --input-provider qmp \
  --power-provider libvirt \
  --libvirt-domain ozma-phase4-libvirt-sys-20261125 \
  --libvirt-uri qemu:///system \
  --http-port 47385 \
  --hid-udp-port 60734 \
  --cap hid,video,power \
  --rtsp-port 0 \
  --webtransport-port 0
```

Startup confirmed the system domain and the shared QMP client:

```text
INFO ozma_qmp::client: QMP control connected path=.../sys/qmp.sock
INFO ozma_power::libvirt: libvirt power: connected
     domain=ozma-phase4-libvirt-sys-20261125 state=running
INFO ozma_node: libvirt power backend connected
```

Live transitions:

```text
GET  /power/state -> 200 {"available":true,"status":"running"}
GET  /display/snapshot -> 200 image/jpeg, 96,470 bytes, 1280x800

POST /power/pause  -> 200 {"action":"pause","ok":true}
virsh domstate     -> paused
GET  /power/state  -> 200 {"available":true,"status":"paused"}

POST /power/resume -> 200 {"action":"resume","ok":true}
virsh domstate     -> running
GET  /power/state  -> 200 {"available":true,"status":"running"}

POST /power/stop   -> 200 {"action":"force-off","ok":true}
virsh domstate     -> shut off
GET  /power/state  -> 200 {"available":true,"status":"shutoff"}
QMP socket         -> absent

POST /power/start  -> 200 {"action":"on","ok":true}
virsh domstate     -> running
QMP socket         -> recreated, mode 0775, owner libvirt-qemu
GET  /power/state  -> 200 {"available":true,"status":"running"}
```

### 4.1 System QMP socket permission finding

The system QEMU runs as `libvirt-qemu`. Its extra QMP socket was created:

```text
srwxrwxr-x 1 libvirt-qemu libvirt-qemu ... sys/qmp.sock
```

The unprivileged node user could not connect (`PermissionError [Errno 13]`).
After `sudo chmod 0777` on that temporary socket, the node connected and
`/display/snapshot` returned a real frame. After the domain was stopped and
started, the socket was recreated 0775 again:

```text
POST /power/start -> 200
virsh domstate    -> running
QMP socket        -> srwxrwxr-x libvirt-qemu:libvirt-qemu
GET /display/snapshot -> 503 (node cannot open the recreated socket)
sudo chmod 0777 .../sys/qmp.sock
GET /display/snapshot -> 200 image/jpeg, 20,393 bytes, 720x400
GET /power/state       -> 200 {"available":true,"status":"running"}
```

Power control continued to work while capture was 503, because `LibvirtPower`
shells out to `virsh` and does not need the extra QMP socket. This is a real
deployment gap, not a fabricated success: the node correctly reported capture
unavailable instead of returning a stale image.

## 5. Invalid-domain error path

The original run started a node with a domain that does not exist on a separate
port with no QMP socket. It failed honestly but degraded to `not-configured`,
which hid the real reason. That gap is fixed: `virsh` "failed to get domain" is
now classified as `domain-not-found`, the node retains the failure in an
unavailable backend, and `/power/state` plus every action return the typed
reason. Startup remains non-fatal.

Live re-validation command (system URI made explicit):

```bash
RUST_LOG=info ./target/debug/ozma-node \
  --name ozma-libvirt-invalid \
  --profile vm \
  --power-provider libvirt \
  --libvirt-domain ozma-does-not-exist-20261125 \
  --libvirt-uri qemu:///system \
  --http-port 47393 \
  --hid-udp-port 60743 \
  --cap hid,power \
  --rtsp-port 0 \
  --webtransport-port 0
```

Typed startup failure (not a fake connected backend, and not `not-configured`):

```text
WARN ozma_node: libvirt power backend unavailable; /power/* will report the
     retained error domain="ozma-does-not-exist-20261125"
     uri="qemu:///system" kind="domain-not-found"
     error=libvirt domain "ozma-does-not-exist-20261125" was not found:
     error: failed to get domain 'ozma-does-not-exist-20261125'
```

Endpoints show the real reason:

```text
GET  /power/state -> 503 {"available":false,"status":"unavailable",
                          "code":"domain-not-found",
                          "error":"libvirt power backend is unavailable:
                          libvirt domain \"ozma-does-not-exist-20261125\" was
                          not found: error: failed to get domain ..."}
POST /power/start -> 503 {"ok":false,"action":"on","code":"domain-not-found",
                          "error":"..."}
POST /power/off   -> 503 {"ok":false,"action":"off","code":"domain-not-found",
                          "error":"..."}

$ virsh -c qemu:///system domstate ozma-does-not-exist-20261125
error: failed to get domain 'ozma-does-not-exist-20261125'
```

Missing `virsh` (`missing-command`) and a failed libvirt connection
(`command-failed`) remain distinct codes; only a missing-domain message maps to
`domain-not-found`. The node stays up and reports power as unavailable; it does
not exit and it does not claim a running state.

## 6. Graceful node shutdown

The invalid-domain node and the system-domain node were stopped with SIGTERM.
Both logged the expected shutdown sequence and closed their listeners:

```text
INFO ozma_node: shutdown requested; stopping node tasks signal="SIGTERM"
INFO ozma_node: Ozma node shutting down

invalid node: exited in 104 ms
system node:  exited in  53 ms
HTTP ports 47383 / 47385: closed
```

The system node was running the full libvirt + QMP capture/input path against a
running domain at the time; its libvirt domain remained `running` after the
node exited, which confirms node shutdown does not forcibly stop the VM.

The first (session) node was already gone when the shutdown loop reached it:
`kill: (345320) - No such process` at 10:36:19Z, and its log ends at the last
input event (10:34:32Z) with no shutdown line. A parallel `cargo test -p
ozma-node --lib` and other workstream activity were present during the run, so
this is recorded as an environment anomaly rather than a node-shutdown result.
The system node, running the same binary and the same power/capture/input
configuration, provides the graceful-shutdown evidence.

## 7. Cleanup

Both disposable domains were destroyed and undefined by exact name:

```bash
virsh -c qemu:///session destroy ozma-phase4-libvirt-live-20261125
virsh -c qemu:///session undefine ozma-phase4-libvirt-live-20261125
virsh -c qemu:///system  destroy ozma-phase4-libvirt-sys-20261125
virsh -c qemu:///system  undefine ozma-phase4-libvirt-sys-20261125
```

Post-cleanup verification:

```text
virsh -c qemu:///session list --all   -> empty
virsh -c qemu:///system  list --all   -> only the five pre-existing shut-off
                                         domains remain
virsh -c qemu:///system  net-list     -> default active/yes/yes
systemctl is-active libvirtd          -> active
systemctl is-enabled libvirtd         -> enabled

/tmp/opencode/ozma-libvirt-live       -> removed
libvirt domain logs                   -> removed
ozma-node processes                   -> none
qemu-system processes                 -> none
test TCP/UDP ports                    -> closed
/tmp/ozma-*.qmp                       -> none
ozma uinput devices (/proc/bus/input) -> none
```

A VM-profile node started without an explicit QMP input provider (the
invalid-domain node uses the default `uinput` chain) can create temporary
`ozma-keyboard`/`ozma-mouse` uinput devices. Such devices were observed during
the run and were gone after all test nodes exited (verified in
`/proc/bus/input/devices`). No NVRAM or disk image existed to remove (the test
domain had no writable disk and no `<nvram>`).

## 8. Sudo and persistent changes

All `virsh` commands ran as user `matt` through the session connection or the
`libvirt` group; no `sudo virsh` was used.

Sudo commands executed:

1. `sudo chmod 0777 /tmp/opencode/ozma-libvirt-live/sys/qmp.sock`
   — after the initial system-domain start, to let the unprivileged node open
   the system QEMU's extra QMP socket.
2. `sudo chmod 0777 /tmp/opencode/ozma-libvirt-live/sys/qmp.sock`
   — after the system-domain stop/start, because libvirt recreated the socket
   with mode 0775.
3. `sudo rm -f /var/log/libvirt/qemu/ozma-phase4-libvirt-sys-20261125.log`
   — remove the root-owned system-domain libvirt log.
4. Read-only inspection: `sudo dmesg --ctime`, `sudo ls -l
   /var/log/libvirt/qemu/`, `sudo find /var/lib/libvirt ...`. These made no
   changes.

Persistent changes after cleanup: **none**. The chmod targets were temporary
sockets inside the removed temp directory. No package was installed, no
service was enabled/started/stopped, no network was modified, and no
pre-existing domain was touched.

## 9. Remaining gaps

1. **libvirt URI configuration — RESOLVED (2026-11-25).** The node now has
   `--libvirt-uri` / `OZMA_LIBVIRT_URI`, and `LibvirtPower` passes
   `virsh --connect <uri>` on state and every action. When unset, `--connect`
   is omitted and `virsh` keeps its own default (`qemu:///session` here).
   Covered by exact-argv unit tests and a live `qemu:///system` run.
2. **System-domain QMP socket permissions — OPEN.** A `-qmp` socket added through
   `qemu:commandline` is created mode 0775, owner `libvirt-qemu`. An
   unprivileged node cannot connect. Production needs a documented ACL/umask
   strategy, a libvirt-managed monitor path, or a node-side check that reports
   the permission error during startup. Today capture correctly returns a typed
   503, but only after the operator notices.
3. **`qemu:commandline` is not a stable libvirt interface — OPEN.** It worked on
   libvirt 12.9.0 / QEMU 11.1.3, but it bypasses libvirt's supported device
   model and may break on upgrades. A supported way to expose a second QMP
   socket (or use libvirt's monitor) should be chosen and regression-tested.
4. **Single-client QMP socket — OPEN (documentation).** The node's shared
   control client is the only QMP client; an independent `socat`/Python QMP
   probe times out while the node is running. Operational verification must use
   libvirt state and node logs. This is expected, but it should be documented
   for support and tests.
5. **Power action result semantics — RESOLVED for ACPI `off`.** The libvirt
   backend now sends `virsh shutdown`, polls `domstate` for up to 3 s, and
   returns HTTP 202 with `pending:true`, `reason:"acpi-requested"` and the real
   observed state when the guest has not shut down. It returns 200 with
   `status:"shutoff"` only when `domstate` says the domain is off, and it is
   idempotent for an already-off domain. `start` is still really
   resume-then-start; consumers should not treat HTTP 200 as "state changed"
   for other actions.
6. **Invalid domain — RESOLVED.** A typo in `--libvirt-domain` is classified as
   `domain-not-found`, retained in an unavailable backend, and reported by
   `/power/state` and actions as HTTP 503 `status:"unavailable"` with the
   original error. Missing `virsh` (`missing-command`) and a failed connection
   (`command-failed`) stay distinct. Startup remains non-fatal.
7. **Capture geometry reporting — RESOLVED.** Before the first frame,
   `/display/info` now reports `width:0`, `height:0`,
   `dimensions_source:"unknown"` and exposes capability limits separately as
   `capability_max_width` / `capability_max_height`. After a real frame it
   reports `last-frame` with the frame geometry. The snapshot itself is always
   the real frame.
8. **TCG in this test — OPEN.** The domains used `<domain type='qemu'>`, so
   libvirt launched QEMU with `-accel tcg`. KVM (`type='kvm'`) was not
   exercised. The power/capture code paths are accelerator-independent, but a
   KVM rerun would be useful for timing.
9. **Out of scope here:** HLS/WebRTC/audio, VNC capture, the unmodified Python
   controller, dashboard rendering, guest OS shutdown handling, and per-VM
   supervisor provisioning. This report validates only the libvirt power +
   QMP capture/input path.
