# Phase 4 systemd runtime and udev smoke report

Date: 2026-11-25 20:53–20:56 (Australia/Sydney)
Host: `SFFPC`, Linux 7.3.5-2-cachyos, systemd 262 (262.2-1-arch), x86_64
Tree: `/home/matt/ozma/ozma` at `4547cd4` **plus uncommitted work**. No commit,
tag, or worktree was created.
Status: **host integration applied; packaged `ozma-node@vm.service` ran as
`ozma-node` with fake capture and stopped cleanly; no unit enabled or left
running; no desktop/GPU/audio/USB capture; AppArmor and `qemu.conf` deliberately
untouched.**

This closes the "no node service was started" and "udev rules not applied"
runtime gaps in [`phase4-privileged-install-report.md`](phase4-privileged-install-report.md)
§10 for the fake-source path only. It does not validate real V4L2, PipeWire,
QMP/VNC, HID-gadget, or USB-gadget data.

## 1. Scope and safety constraints

- Used the installed release `ozma-node` and the installed packaged unit; no
  rebuild, install, enable, or persistent service start.
- Fake capture (`--capture-provider fake`), recording input
  (`--input-provider recording`), and `none` for audio/display/power. No
  `--qmp-socket`, `--qmp-input-socket`, `--vnc-*`, `--hls-*` hardware encoder,
  or `--usb-gadget` flags were passed.
- No `--register-url`, so no controller registration or heartbeat POST. The node
  still made its normal local mDNS advertisement while running.
- Applied only the Ozma-installed udev rules/tmpfiles/modules-load assets.
  Unrelated udev rules and device nodes were not touched.
- Did not load AppArmor or edit `/etc/libvirt/qemu.conf`.
- No service is enabled or running after the smoke; temporary env, HLS output,
  sockets, and logs were removed.

## 2. Pre-state

| Item | Pre-state |
|---|---|
| Account | `ozma-node` uid/gid 942; groups `input kvm render video libvirt` |
| Units | four Ozma units installed, all `disabled`; no active Ozma unit |
| `/dev/uinput` | `0660 root:input`, ACL `matt`, `brltty` |
| `/dev/dri/renderD128-130` | `0666 root:render` |
| `/dev/dri/card0-3` | `0660 root:video`; card0 ACL `matt` |
| `/dev/input/event0` | `0660 root:input`, ACL `matt` |
| `/dev/video0-*` | `0660 root:video` |
| `/dev/hidg*` | absent; no ConfigFS gadget/UDC |
| Modules | `uinput` loaded; `libcomposite` absent |
| tmpfiles dirs | `/run/ozma` `0755`, `/run/ozma-node` `0750`, `/run/ozma/qmp` `0770 root:kvm`; state dirs `0750 ozma-node:ozma-node` |
| AppArmor | `apparmor.service` not found, `apparmor_parser` absent |
| `qemu.conf` | `sha256 2c7a8e5f199f87c6ea6896e638c8df34aca88e07bff7a54855db6e20678e3e55` |

## 3. Applied host integration (every mutating sudo command)

```bash
sudo -n systemd-tmpfiles --create --dry-run /etc/tmpfiles.d/ozma-node.conf
sudo -n systemd-tmpfiles --create /etc/tmpfiles.d/ozma-node.conf
sudo -n modprobe uinput
sudo -n modprobe libcomposite
sudo -n udevadm control --reload-rules
sudo -n udevadm trigger --action=change --subsystem-match=misc --sysname-match=uinput
sudo -n udevadm trigger --action=change --subsystem-match=hidg
sudo -n udevadm trigger --action=change --subsystem-match=video4linux --sysname-match='video*'
sudo -n udevadm trigger --action=change --subsystem-match=drm --sysname-match='renderD*'
sudo -n install -m 0600 -o root -g ozma-node <tmp>/ozma-node-vm.env /etc/ozma/ozma-node-vm.env
sudo -n systemctl start ozma-node@vm.service
sudo -n systemctl stop ozma-node@vm.service
sudo -n rm -f /etc/ozma/ozma-node-vm.env
sudo -n rm -rf /var/lib/ozma-node/hls/ozma-smoke
```

Additional read-only `sudo` use was limited to `stat`/`ls`/`find`/`journalctl`,
`systemctl show/status`, and `sudo -u matt|ozma-node` open/close permission
tests. The open tests did not read frames/events or issue ioctls.

### 3.1 tmpfiles

The dry run reported `Would create directory` for `/run/ozma-node`, `/run/ozma`,
and `/run/ozma/qmp` (all already existed), then `--create` returned 0. Resulting
modes/owners were unchanged: `/run/ozma` `0755 ozma-node:ozma-node`,
`/run/ozma-node` `0750 ozma-node:ozma-node`, `/run/ozma/qmp` `0770 root:kvm`.
State directories `/var/lib/ozma-node`, `/var/lib/ozma-node/hls`, and
`/var/lib/ozma` stayed `0750 ozma-node:ozma-node`.

### 3.2 Modules

`uinput` was already loaded (refcount 3). `modprobe libcomposite` loaded
`libcomposite` (114,688 bytes, refcount 0). Both are the only modules listed in
`/etc/modules-load.d/ozma-node.conf`; no other module was loaded. No gadget or
UDC function was configured, so no `/dev/hidg*` node appeared.

### 3.3 udev

`udevadm control --reload-rules` returned 0. Triggers were deliberately
targeted; the input subsystem was **not** triggered, so the user's existing
`/dev/input/event*` devices were not reset. `hidg` had no matching devices, so
its trigger was a no-op.

| Device | Before | After |
|---|---|---|
| `/dev/uinput` | `0660 root:input`, ACL `matt`, `brltty` | `0660 root:ozma-node`, ACL `matt`, `brltty` preserved |
| `/dev/dri/renderD128-130` | `0666 root:render` | `0660 root:render` |
| `/dev/dri/card0-3` | `0660 root:video` | unchanged |
| `/dev/input/event0` | `0660 root:input`, ACL `matt` | unchanged |
| `/dev/video0-*` | `0660 root:video` | unchanged |
| `/dev/hidg*` | absent | absent |

The `uinput` and `render` changes are the intended persistent policy from the
installed rules, so they were left in place.

## 4. Service runtime

### 4.1 Unit choice

The packaged `ozma-node@vm.service` was preferred over a synthetic
`systemd-run` unit. It exercises the real installed unit file, `User=`/`Group=`,
hardening, `DeviceAllow=`, `EnvironmentFile=`, and the USB-gadget drop-in. The
template reads the fixed path `/etc/ozma/ozma-node-vm.env`, so a temporary
`0600 root:ozma-node` env file supplied all run-specific flags. The unit was
started directly with `systemctl start` and never enabled.

Difference from `systemd-run`: a transient unit would prove the binary/account
but bypass the packaged `EnvironmentFile=` and its hardening defaults; the
packaged unit is the stronger test. The fixed env path is also why this needs a
temporary file instead of command-line properties.

### 4.2 Temporary env (token redacted)

```ini
OZMA_NODE_ARGS=--name ozma-smoke --capture-provider fake --fake-width 96 --fake-height 64 --fake-fps 2 --input-provider recording --audio-provider none --display-provider none --power-provider none --http-port 34383 --hid-udp-port 43789 --api-token <temporary> --hls --hls-dir /var/lib/ozma-node/hls/ozma-smoke --hls-fps 2 --hls-bitrate-kbps 200 --hls-encoder libx264
RUST_LOG=ozma_node=info,ozma_capture=info
```

### 4.3 Start and API evidence

```text
Started Ozma node (vm profile) at 20:54:46.
WARN DEV/TEST SYNTHETIC CAPTURE: --capture-provider fake ... width=96 height=64 fps=2
INFO Input sink ready sink="fake-recording"
INFO HLS pipeline started ... encoder="libx264"
INFO UDP HID compatibility listener running port=43789
INFO mDNS advertising ozma-smoke._ozma._udp.local.
INFO Native QUIC session listener ready endpoint=192.168.40.19:34385
INFO Node API listening port=34383
```

| Check | Result |
|---|---|
| `/health` (bearer token) | `{"ok":true,"node_id":"ozma-smoke._ozma._udp.local.","version":"0.1.0","protocol":{"major":1,"minor":0},"sessions":0}` |
| `/api/v1/connection` | `profile:"vm"`, `capture.provider:"fake"`, `hls:"running"`, `input.sink_kind:"fake-recording"`, `power:"not-configured"`, `audio:"not-configured"`, `usb.status:"not-configured"` |
| Unauthenticated `/api/v1/connection` | HTTP 401 |
| `/video` | MJPEG stream (`--ozma-frame`, `image/jpeg`); the bounded curl timed out by design after receiving frames |
| `/snapshot` | HTTP 200, `image/jpeg`, 96×64, 771 bytes (fake source) |
| `/stream/stream.m3u8` | HLS manifest with 1 s segments (`seg_00009.ts` …) |
| Listener/process | TCP `0.0.0.0:34383`, UDP `0.0.0.0:43789`, main PID owned by `ozma-node:ozma-node` |
| Unit properties | `User/Group=ozma-node`, `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `PrivateDevices=no`, `DeviceAllow=/dev/{video*,hidg*,uinput,input/event*,dri/renderD*} rw` |

The journal shows the expected `UAC2 ConfigFS function not found` and
`UAC2 ALSA device not found` warnings because no USB gadget exists; these are
typed not-configured states, not capture of host audio.

### 4.4 Stop and cleanup

`systemctl stop` sent SIGTERM at 20:55:08. The journal records
`shutdown requested ... signal="SIGTERM"`, `Ozma node shutting down`,
`Deactivated successfully`, and `Stopped Ozma node (vm profile)` after 21.9 s
wall time, 432 ms CPU, 124.6 M peak.

After stop:

- `systemctl is-active` → `inactive`; `is-enabled` → `disabled`.
- No `ozma-node` process; no listener on 34383/43789.
- No socket or file under `/run/ozma` or `/run/ozma-node`; tmpfiles directories
  remain intact with the installed modes/owners.
- `/etc/ozma/ozma-node-vm.env` and `/var/lib/ozma-node/hls/ozma-smoke` were
  removed. `/etc/ozma` is back to the five examples, operator file, and libvirt
  snippet; `/var/lib/ozma-node/hls` is empty.
- All smoke logs/evidence under `/tmp/opencode/ozma-systemd-smoke` were removed
  after this report was written.

## 5. Device and user-impact verification

Open/close tests (no reads or ioctls):

| Account | Device | Result |
|---|---|---|
| `matt` | `/dev/dri/card0` | OK |
| `matt` | `/dev/input/event0` | OK |
| `matt` | `/dev/uinput` | OK (ACL preserved despite group change) |
| `matt` | `/dev/dri/renderD128` | **denied** (EACCES) |
| `ozma-node` | `/dev/dri/renderD128` | OK |
| `ozma-node` | `/dev/uinput` | OK |
| `ozma-node` | `/dev/dri/card0` | OK |

**Finding:** the installed render rule changes render nodes from world-writable
`0666` to `0660 root:render`. The service account is in `render` and works, but
the desktop user `matt` is not in `render`, so applications running as `matt`
that open render nodes directly now get EACCES. Display `card*` nodes and input
event nodes retain their `matt` ACLs and still open. This is the rules' intended
policy, not a device reset; if the desktop needs direct render access, add
`matt` to `render` as a separate operator decision (not done here). Leaving
render at `0666` would contradict the installed policy.

## 6. Persistent and runtime changes

Persistent/intended:

- Installed udev rules are now applied to the running device tree:
  `/dev/uinput` group `ozma-node`, render nodes `0660 root:render`. The rules
  remain in `/etc/udev/rules.d` and reapply at boot.
- `/etc/modules-load.d/ozma-node.conf` will load `uinput` and `libcomposite` at
  boot; `libcomposite` is loaded now at runtime (refcount 0).
- tmpfiles directories were created/confirmed from the installed snippet.
- No new service, unit override, env file, user, or group was created. No unit
  is enabled.

Runtime-only and removed:

- Temporary `/etc/ozma/ozma-node-vm.env`, HLS output, sockets, process,
  listeners, and logs.
- `libcomposite` remains loaded until reboot (intended by modules-load).

Deliberately not done:

- **AppArmor:** `apparmor.service` is not installed (`could not be found`) and
  `apparmor_parser` is absent, so the installed drop-in was not parsed/loaded.
  Loading another LSM profile would affect the host's libvirt/QEMU policy and is
  out of scope for a fake-source node.
- **libvirt `qemu.conf`:** the installer is copy-only by design; merging the
  `cgroup_device_acl` snippet would affect real VMs and is unnecessary for this
  path. The file's SHA-256 is unchanged.
- **Real capture/input:** no V4L2, PipeWire, audio, USB-gadget, `/dev/hidg*`,
  QMP, VNC, or real uinput device was opened or captured.

## 7. Remaining gaps

1. Desktop user `matt` is not in `render`; direct render-node access is now
   denied by the intended udev policy. Decide whether to add the user to the
   group.
2. No real hardware-profile runtime: V4L2 capture, HID-gadget `/dev/hidg*`,
   ConfigFS/UDC setup, and gadget audio were not exercised.
3. No real VM runtime: QMP/D-Bus/VNC capture, QMP input, and QMP/libvirt power
   were not opened; the node used fake capture, recording input, and no power.
4. No desktop runtime: PipeWire/portal/X11 capture, audio routing, and real
   `uinput` injection were not exercised on the user's session.
5. AppArmor drop-in remains unparsed; libvirt `cgroup_device_acl` remains
   copy-only and unmerged.
6. The packaged template hardcodes `/etc/ozma/ozma-node-vm.env`; per-run
   overrides need a temporary env file or a drop-in. The smoke used and removed
   a temporary file.
7. Evidence is for a dirty tree (`4547cd4` + uncommitted work), not a frozen
   release tag.
8. Local mDNS advertisement occurred during the smoke (no controller was
   configured). No registration/heartbeat request was made.
