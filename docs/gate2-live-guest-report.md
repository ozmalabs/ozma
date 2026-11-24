# Gate 2 live guest-visible report

**Scope:** guest-visible display, keyboard input, power, and audio against a
real, disposable Alpine Linux guest, plus the Rust node's shared-QMP/HLS,
snapshot, input, and power paths and a live QEMU D-Bus display.

**Result:** a real guest booted and guest-visible evidence was captured for
display (QMP screendump, node snapshots, HLS, D-Bus), keyboard input (manual
QMP and node QMP `/input/key`), and power (pause/resume/reset). Audio is
**SKIP**: this QEMU instance had no audio device or backend, and the guest has
no `/dev/snd`. Controller/dashboard is **SKIP** because an unrelated controller
from another session owns the controller's fixed auxiliary ports and
`/dev/video0`; starting a second one would interfere.

**Post-fix status (2026-11-24 17:16–17:19 AEDT):** the HLS resolution-change
failure in §9 is now **fixed and live-verified** on the same disposable Alpine
guest. Three node-issued resets reproduced `1280x800 -> 720x400 -> 1280x800`
each time; HLS stayed `running`, the playlist advanced across two
`#EXT-X-DISCONTINUITY` markers per reset, and old/new segments decoded at
720x400 and 1280x800 respectively. See [§0](#0-post-fix-live-rerun). The
pre-fix sections below are retained as history.

**This is not a Gate 2 claim.** It closes the previous "no guest OS" blocker for
the paths below. Audio, mouse-pointer, controller/dashboard, D-Bus live
mode-transition rerun, and WebRTC-media gaps remain.

> No `sudo`, no libvirt domain, no `virsh` mutating verb, no network download,
> and no package install were used. The only VM was a direct headless QEMU
> process started from a read-only Alpine ISO with no writable disk. No
> existing libvirt image was opened for write. The original ISO/hash is
> unchanged. All QEMU/node processes, sockets, ports, and temporary files were
> removed; see [Cleanup](#cleanup).

## 0. Post-fix live rerun

**Result: PASS for the QMP HLS resolution-change path.** The same read-only
Alpine live ISO was booted with QMP/VGA/serial, the node ran with shared QMP and
`--input-provider recording --power-provider qmp --hls --hls-fps 5`, and the
guest was reset three times with `POST /power/reset`. Each reset reproduced
`1280x800 -> 720x400 -> 1280x800`; HLS stayed `running`, the playlist advanced
with a `#EXT-X-DISCONTINUITY` at both switches, and segments decoded at their
own resolutions.

### 0.1 Build and run metadata

| Item | Value |
|---|---|
| Run | 2026-11-24 17:16–17:19 AEDT (2026-11-24T06:16–06:19Z) |
| HEAD | `27ccbe1113b840ddf05436bc0cf88e9b4cc4d514` (`27ccbe1`) |
| Build | `cargo build -p ozma-node` exited 0; Cargo reported the crate up to date |
| Node binary | `target/debug/ozma-node`, 282,311,152 bytes, `ozma-node 0.1.0` |
| QEMU | `qemu-system-x86_64` 11.1.3, KVM (`accel=kvm:tcg`) |
| ffmpeg | n9.1 |
| Guest | Alpine Linux 3.21 live, kernel `6.12.13-0-virt`, root in tmpfs |
| Temp root | `/tmp/opencode/ozma-gate2-guest-postfix-20261124-171636` (removed) |
| Node ports | HTTP 47341, QUIC/UDP 47343, HID/UDP 60341 |

The working tree was dirty from concurrent sessions (controller, power, QMP,
and docs edits). This rerun did not edit or commit those files; it only updated
this report.

### 0.2 Boot and node command

```bash
ROOT=/tmp/opencode/ozma-gate2-guest-postfix-20261124-171636
ISO='/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso'
xorriso -osirrox on -indev "$ISO" \
  -extract /boot/vmlinuz-virt "$ROOT/qemu/vmlinuz-virt" \
  -extract /boot/initramfs-virt "$ROOT/qemu/initramfs-virt"

qemu-system-x86_64 \
  -name ozma-gate2-guest-postfix \
  -machine pc,accel=kvm:tcg -cpu host -smp 2 -m 512 \
  -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" \
  -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modules=loop,squashfs,sd-mod,usb-storage alpine_dev=cdrom" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -device VGA -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 \
  -display none \
  -serial "file:$ROOT/qemu/serial.log" \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" -daemonize

./target/debug/ozma-node \
  --name ozma-gate2-guest-postfix \
  --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --input-provider recording \
  --power-provider qmp \
  --http-port 47341 \
  --hid-udp-port 60341 \
  --cap hid,video \
  --hls \
  --hls-dir "$ROOT/node/hls" \
  --hls-fps 5 \
  --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

Baseline before reset: `/display/snapshot` was a valid 1280×800 JPEG (97,897
bytes, SHA-256 `78247e0e…2519`); `/api/v1/connection` reported
`hls: "running"`, `hls_error: null`, `power.status: "running"`.

### 0.3 Live reset transitions

Three resets were issued through the node:

```bash
curl -sS -X POST http://127.0.0.1:47341/power/reset
# {"action":"reset","ok":true}
```

| Reset | Start time | Observed sequence | HLS status |
|---|---|---|---|
| 1 | 06:17:11Z | 1280×800 → 720×400 → 1280×800 | `running` throughout |
| 2 | 06:17:58Z | 1280×800 → 720×400 → 1280×800 | `running` throughout |
| 3 | 06:18:49Z | 1280×800 → 720×400 → 1280×800 | `running` throughout |

A 50 ms `/proc/*/cmdline` watcher captured the restart command. The 720×400
generation was:

```text
/usr/bin/ffmpeg -hide_banner -loglevel warning -f rawvideo -pix_fmt rgb24 \
  -s 720x400 -r 5 -i - -an -c:v libx264 -preset veryfast -tune zerolatency \
  -b:v 1000k -pix_fmt yuv420p -g 5 -keyint_min 5 -sc_threshold 0 \
  -force_key_frames expr:gte(t,n_forced*1) -f hls -hls_time 1 -hls_list_size 4 \
  -hls_flags delete_segments+append_list+discont_start \
  -hls_segment_filename .../node/hls/seg_%05d.ts .../node/hls/stream.m3u8
```

The generation before and after it used `-s 1280x800` with the same flags. The
resumed 1280×800 generation was the live ffmpeg child after each reset.

### 0.4 Playlist and discontinuity evidence

Playlist snapshot from the first reset, with both switches:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:19
#EXTINF:1.000000,
seg_00019.ts
#EXT-X-DISCONTINUITY
#EXTINF:1.000000,
seg_00020.ts
#EXTINF:1.000000,
seg_00021.ts
#EXT-X-DISCONTINUITY
#EXTINF:1.000000,
seg_00022.ts
```

Playlist snapshot from the second reset:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:65
#EXTINF:1.000000,
seg_00065.ts
#EXT-X-DISCONTINUITY
#EXTINF:1.000000,
seg_00066.ts
#EXTINF:1.000000,
seg_00067.ts
#EXT-X-DISCONTINUITY
#EXTINF:1.000000,
seg_00068.ts
```

The monitor recorded media-sequence advance from 14 to 36 during the first
20-second window and from 65 to 79 during the second. The discontinuity lines
scrolled out of the 4-segment live window after the switch; the snapshots above
are the live playlists at the transition.

### 0.5 Segment resolutions (ffprobe)

| Segment | Resolution | Size | SHA-256 |
|---|---|---|---|
| `seg_00065.ts` | h264 1280×800 | 98,324 | `34d00bdd…2cf7` |
| `seg_00066.ts` | h264 720×400 | 283,880 | `46c79f3d…fd42` |
| `seg_00067.ts` | h264 720×400 | 175,216 | `e3728a4a…5f08` |
| `seg_00068.ts` | h264 1280×800 | 84,224 | `48998a69…4c17` |

The first reset produced the same pattern with `seg_00019.ts` (1280×800),
`seg_00020.ts`/`seg_00021.ts` (720×400), and `seg_00022.ts` (1280×800). Every
archived segment was a valid MPEG-TS with one H.264 video stream.

Frame OCR from the second reset:

```text
seg_00065.ts (1280x800): Alpine boot/login console
seg_00066.ts (720x400):  SeaBIOS (version Arch Linux 1.17.0-2-2)
seg_00068.ts (1280x800): Alpine kernel boot: Mounting modloop ... [ ok ]
```

This proves both directions: the pipeline scaled down `1280x800 -> 720x400`
and back up `720x400 -> 1280x800` on every reset.

### 0.6 Snapshots across the transition

A 150 ms snapshot poller run across the third reset captured:

| Time after reset | Snapshot | Size |
|---|---|---|
| 0.159 s | `snapshot_1280x800.jpg`, valid JPEG | 97,887 bytes |
| 2.183 s | `snapshot_720x400.jpg`, valid JPEG; OCR `SeaBIOS` | 8,336 bytes |
| after reset | `after-reset3.jpg`, valid JPEG 1280×800 | 97,897 bytes |

Hashes: 1280×800 `485db7e8…de7`; 720×400 `d2c7c013…5f35`. All were real JFIF
JPEGs, not empty placeholders. Snapshots did not fail while HLS was restarting.

### 0.7 HLS status and harness

A 200 ms connection poller recorded `hls: "running"`, `hls_error: null`, and
`power.status: "running"` for the entire window before and after all resets;
there was no `failed` interval. The node log had no `error`, `fail`, or `panic`
lines.

After the resets, `scripts/gate2/check-node.sh` passed:

```text
PASS  /health returned 200 and ok=true
PASS  /api/v1/connection returned 200 ... (profile 'vm')
PASS  /power/state reports available=true (status 'running')
PASS  /stream/stream.m3u8 returned 200 and starts with #EXTM3U
PASS  HLS playlist advanced within 1s
SKIP  WebRTC offer POST disabled (OZMA_GATE2_SKIP_WEBRTC_OFFER=1)
SKIP  controller checks disabled (OZMA_CONTROLLER_URL is not set)
Summary: 5 passed, 0 failed, 0 warnings, 2 skipped
```

### 0.8 Post-fix cleanup

| Item | Final state |
|---|---|
| QEMU (PID 4169498) | `quit`; exited |
| Node and HLS ffmpeg | SIGINT; node logged `Ozma node shutting down`; ffmpeg reaped |
| Temp root `/tmp/opencode/ozma-gate2-guest-postfix-20261124-171636` | removed |
| Monitor/helper scripts | removed |
| Ports 47341/47343/60341 | no listeners |
| Original Alpine ISO | unchanged; SHA-256 `f28171c3…90f2d` |

No `sudo`, no writable disk, no libvirt domain, no network download, and no
commit were used. The only repository change from this rerun is this report.

## 1. Run metadata

| Item | Value |
|---|---|
| Run | 2026-11-24 14:41–14:50 AEDT (2026-11-24T03:41–03:50Z) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD | `436b80c7b384b80e8db9163ad294af571f06641a` (`436b80c`) |
| Node binary | `target/debug/ozma-node`, 281,384,240 bytes, version `0.1.0` |
| QEMU | `qemu-system-x86_64` 11.1.3 |
| Acceleration | KVM: `query-kvm` returned `{"enabled":true,"present":true}` on both VMs |
| Host | 12 CPUs, 30 GiB RAM, Linux |
| ffmpeg | n9.1 |
| Guest | Alpine Linux 3.21 live, kernel `6.12.13-0-virt`, root in tmpfs |
| Temp root | `/tmp/opencode/ozma-gate2-guest-20261124-144126` (removed) |
| Node ports | HTTP 47321, QUIC/UDP 47323, HID/UDP 60321 |
| D-Bus node ports | HTTP 47331, QUIC/UDP 47333, HID/UDP 60331 |

The working tree changed during the run from concurrent sessions (modified
`controller/api.py`, `ozma-capture/src/dbus_display.rs`, `ozma-node/src/hls.rs`,
`scripts/gate2/webrtc-client.py`, `softnode/*`, plus an unrelated
`.gate2-evidence-preview.png`). This validation did not edit, build, or commit
those files; it only added this report.

## 2. Guest asset inventory (read-only)

| Asset | Size / mode | Usable? |
|---|---|---|
| `/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso` | 66,060,288 bytes, mode 644 | **Used.** Bootable live ISO (kernel, initramfs, modloop, apks). |
| same ISO SHA-256 | `f28171c35bbf623aa3cbaec4b8b29297f13095b892c1a283b15970f7eb490f2d` | Unchanged before/after. |
| `/boot` | directory unreadable to this user (`Permission denied`) | Not usable without sudo. |
| `/usr/lib/modules/7.3.5-2-cachyos/vmlinuz` | 17,874,944 bytes | Host kernel, but no matching user-space root/initramfs was needed. |
| `/usr/lib/modules/6.17.9-2-cachyos/` | modules only; no `vmlinuz`/initramfs found | Not a bootable pair. |
| `/var/lib/libvirt/images/winxp-install.qcow2` | 21,478,375,424 bytes, root-only 0600 | **Inaccessible** without sudo. No other libvirt images. |
| `/home/matt/vms/winxp.qcow2` | 2,419,523,584 bytes, mode 666 | Present but not used: original Windows image, heavy, and not needed. |
| `/home/matt/vms/Windows XP Professional SP3 ... .iso` | 727,515,136 bytes | Present but not used. |
| `/home/matt/ozma/ozmanew/images/riscv-*.qcow2`, `riscv-vmlinuz-*`, `riscv-initramfs-*` | RISC-V assets | Present; not x86_64/KVM-compatible. |
| `busybox`, `dracut`, `mkinitramfs` | **missing** | A minimal initramfs was not needed. |
| `mkinitcpio`, `cpio`, `gzip`, `zstd`, `xorriso`, `mksquashfs` | present | A local initramfs could be built without network if needed. |

A minimal initramfs was therefore buildable in principle, but the Alpine ISO
already provided a complete disposable guest with no writes to any original
image, so no custom initramfs was built.

## 3. Disposable guest boot

The kernel and initramfs were extracted **read-only** from the ISO into the temp
root; the ISO itself was attached as a read-only CD-ROM. No writable disk or
overlay was used, so guest writes stayed in the guest's tmpfs.

```bash
ROOT=/tmp/opencode/ozma-gate2-guest-20261124-144126
ISO='/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso'
mkdir -p "$ROOT"/{qemu,artifacts/ppm}
xorriso -osirrox on -indev "$ISO" \
  -extract /boot/vmlinuz-virt "$ROOT/qemu/vmlinuz-virt" \
  -extract /boot/initramfs-virt "$ROOT/qemu/initramfs-virt"

qemu-system-x86_64 \
  -name ozma-gate2-guest \
  -machine pc,accel=kvm:tcg \
  -cpu host -smp 2 -m 512 \
  -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" \
  -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modules=loop,squashfs,sd-mod,usb-storage alpine_dev=cdrom" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -device VGA \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -device usb-tablet,bus=xhci.0 \
  -display none \
  -serial "file:$ROOT/qemu/serial.log" \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" \
  -daemonize
```

Extracted copies:

| File | SHA-256 |
|---|---|
| `vmlinuz-virt` | `e1d7a3cdae9a4a62ed629b90a9955754f676a339bc28176aa593f8f029c35e3c` |
| `initramfs-virt` | `1713c095eb74d21f9c4fff91680e0db7b578f56969928e16457185b444c387ed` |

Boot reached a login prompt in about 5 s. Serial excerpt:

```text
Welcome to Alpine Linux 3.21
Kernel 6.12.13-0-virt on an x86_64 (/dev/ttyS0)

localhost login:
```

## 4. Guest-visible display and keyboard input — PASS

### 4.1 Manual QMP screendump and keyboard

QMP helper connected to the disposable socket, sent `qmp_capabilities`, then
polled `screendump` and used `send-key` (`QKeyCode`).

| Step | Evidence |
|---|---|
| Initial `screendump` | PPM `P6 1280 800`, 3,072,016 bytes, SHA-256 `12636cd5…ee973`; OCR showed Alpine boot plus `localhost login:` |
| Type `root` + Return | Screenshot SHA changed to `05c570fa…e43`; OCR showed `Welcome to Alpine!` and a shell prompt |
| Type `echo ozma-guest-evidence` + Return | Screenshot SHA changed to `738e07bb…a3`, then `c848399d…abe`; OCR showed the command and output `ozma-guest-evidence` |
| Type `echo ozma-guest-serial-evidence > /dev/ttyS0` | Serial log gained `ozma-guest-serial-evidence` |

Raw OCR of the settled screen (tesseract errors are OCR artifacts, not guest
errors):

```text
localhost:"# echo ozma-guest-evidence
ozna-guest-evidence
localhost:™#
```

This is guest-visible keyboard injection: the guest console changed, the shell
executed the typed command, and a command redirected to the serial console
appeared in the serial log.

### 4.2 Node QMP input

The node was started with `--profile vm --input-provider qmp --power-provider
qmp` and the shared QMP socket. `POST /input/key` returned
`{"ok":true,"sink":"qmp"}` for every key. Typing `root` + Return and then
`echo ozma-node-input` through the node produced a changed snapshot:

```text
localhost:"# echo ozma-node-input
ozna-node- input
localhost:™#
```

The node snapshot was a real 1280×800 JPEG (97,881 bytes before input;
113,192 bytes after, different SHA-256). This is guest-visible input through the
node route, not just `{"ok":true}`.

## 5. Power actions — PASS

### 5.1 Manual QMP pause/resume/reset

| Action | Result |
|---|---|
| `stop` | `query-status` → `{"status":"paused","running":false}` |
| Pause stability | Three PPMs (before, +2 s, +4 s) had the **same** SHA-256 `1a47abbc…9bd`; serial size stayed 27,821 bytes |
| `cont` | `query-status` → `{"status":"running","running":true}`; screen changed again |
| `system_reset` | Serial grew from 27,821 to 55,577 bytes; screenshot returned to Alpine boot/login |

### 5.2 Node power routes

| Step | Response / evidence |
|---|---|
| `GET /power/state` | `{"available":true,"status":"running"}` |
| `POST /power/stop` | `{"action":"force-off","ok":true}`; state → `paused` |
| Paused snapshots | `power-paused-a.jpg` and `power-paused-b.jpg` were byte-identical, SHA-256 `f07478b9…87c`; serial stayed 55,577 bytes |
| `POST /power/on` | `{"action":"on","ok":true}`; state → `running`; typed `echo ozma-resume-ok` appeared in the guest after resume |
| `POST /power/reset` | `{"action":"reset","ok":true}`; state → `running`; serial grew 55,577 → 83,333 bytes; screenshot showed a fresh Alpine boot/login |

Note: `/power/stop` is still labelled `force-off` but QMP `stop` is a pause, not
a destroy. Hard power-off/destroy was not tested.

## 6. Node shared-QMP, snapshot, and HLS — PASS (historical pre-fix reset caveat)

Node command:

```bash
RUST_LOG=info,ozma_input=debug,ozma_qmp=debug ./target/debug/ozma-node \
  --name ozma-gate2-guest \
  --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 47321 \
  --hid-udp-port 60321 \
  --cap hid,video \
  --hls \
  --hls-dir "$ROOT/node/hls" \
  --hls-fps 5 \
  --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

`/api/v1/connection` reported:

```json
{
  "profile": "vm",
  "sink_kind": "qmp",
  "power": {"available": true, "status": "running"},
  "input": {"sink_kind": "qmp", "available": true, "injecting": true},
  "hls": "running",
  "hls_error": null
}
```

Node snapshot before input: `image/jpeg`, 97,881 bytes, 1280×800, OCR showed
`localhost login:`. Node HLS playlist used 1-second segments. The newest
segment at the input check was `seg_00050.ts`, 120,320 bytes, H.264 1280×800.
Decoding its first frame and OCR-ing it showed the same guest command output:

```text
localhost:"# echo ozma-node-input
ozna-node- input
localhost:™#
```

That is HLS content derived from the running guest's display, not a static
placeholder.

**Historical pre-fix reset failure:** after the guest was reset by the node,
the then-current node failed with:

```text
FAIL  HLS is configured but failed: frame dimensions 720x400 do not match the HLS input 1280x800
```

The QMP screendump transiently changed from the running 1280×800 text mode to
the 720×400 VGA mode during reboot, and the HLS encoder did not recover. The
snapshot route still returned a fresh 1280×800 JPEG. Stopping and restarting the
node after the guest reached its final 1280×800 mode made HLS run again:

```text
PASS  /health returned 200 and ok=true
PASS  /api/v1/connection returned 200 ... (profile 'vm')
PASS  /power/state reports available=true (status 'running')
PASS  /stream/stream.m3u8 returned 200 and starts with #EXTM3U
PASS  HLS playlist advanced within 1s
SKIP  WebRTC offer POST disabled (OZMA_GATE2_SKIP_WEBRTC_OFFER=1)
SKIP  controller checks disabled (OZMA_CONTROLLER_URL is not set)
Summary: 5 passed, 0 failed, 0 warnings, 2 skipped
```

The failure is retained as the historical pre-fix finding in §9; it is not
hidden by the later pass. The post-fix live rerun in §0 now passes the same
`1280x800 -> 720x400 -> 1280x800` transition without a node restart.

## 7. D-Bus display — PASS for snapshots and post-boot HLS

A second disposable QEMU used the same read-only ISO and:

```bash
-display dbus,p2p=yes \
-qmp "unix:$ROOT/dbus-qemu/qmp.sock,server=on,wait=off"
```

Node command (after the guest had booted):

```bash
./target/debug/ozma-node \
  --name ozma-gate2-guest-dbus2 \
  --profile vm \
  --capture-provider dbus \
  --qmp-socket "$ROOT/dbus-qemu/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 47331 \
  --hid-udp-port 60331 \
  --cap hid,video \
  --hls --hls-dir "$ROOT/dbus-node/hls2" \
  --hls-fps 3 --hls-bitrate-kbps 800 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

Live log:

```text
DEBUG ozma_capture::dbus_display: D-Bus display connection authenticated id=dbus0
DEBUG ozma_capture::dbus_display: QEMU D-Bus console id=dbus0 label="VGA" width=1280 height=800
DEBUG ozma_capture::dbus_display: D-Bus listener registered; awaiting Scanout id=dbus0
INFO  ozma_node: HLS pipeline started ...
```

`GET /display/snapshot` returned a real 1280×800 JPEG (97,876 bytes), OCR:

```text
Welcome to Alpine Linux 3.21
Kernel 6.12.13-0-virt on an x86_64 (/dev/tty1)
localhost login:
```

QMP input through the D-Bus node (`POST /input/key`) typed `root` and
`echo ozma-dbus-ok`. The D-Bus snapshot and the D-Bus HLS frame both showed:

```text
localhost:"# echo ozma-dbus-ok
ozna-dbus-ok
localhost:™#
```

D-Bus HLS segment `seg_00024.ts` was 119,004 bytes, H.264 1280×800.

**Historical pre-fix startup failure (D-Bus):** the first D-Bus node start
happened while the guest was still in the initial 640×480 VGA mode. It logged
console properties `640x480`, then received 1280×800 frames and failed HLS with:

```text
hls_error: "frame dimensions 1280x800 do not match the HLS input 640x480"
```

Stopping and restarting the node after boot (at 1280×800) made D-Bus HLS run.
This was the same resolution-change limitation as §6, seen from the D-Bus
provider. It was **not re-run** in the post-fix follow-up, which covered the QMP
reset path only; D-Bus mode-change tolerance remains unproven live.

## 8. Audio — SKIP

The QEMU command deliberately had no `-audiodev`, no HDA/AC97 device, and no
capture backend. The guest therefore had no sound device. Typing
`ls /dev/snd` at the guest shell produced:

```text
ls: /dev/snd: No such file or directory
```

No guest-produced sound existed to observe, and no audio route was claimed.
Node `/audio/*` remains honestly `not-configured`/unimplemented. Audio parity is
still unproven.

## 9. Findings and failures

1. **HLS did not survive a guest resolution change (pre-fix).** QMP HLS failed
   after a node-issued reset when a 720×400 frame arrived in a 1280×800 pipeline
   (`frame dimensions 720x400 do not match the HLS input 1280x800`). D-Bus HLS
   failed when started in 640×480 and then received 1280×800 frames
   (`640x480` vs `1280x800`). A node restart after the mode settled recovered
   both. **Follow-up:** `ozma-node/src/hls.rs` now closes and reaps the old
   ffmpeg child and starts a new generation with the new `-s WxH` whenever the
   frame size changes and `HlsConfig::width`/`height` are unset. The restart
   appends to the existing playlist with
   `-hls_flags delete_segments+append_list+discont_start`, marks the switch as
   `#EXT-X-DISCONTINUITY`, and rebases `FramePacer`. Pinned dimensions instead
   produce a typed `HlsError::FrameDimensions`. **Post-fix live result
   (2026-11-24 17:16–17:19 AEDT):** three QMP resets each reproduced
   `1280x800 -> 720x400 -> 1280x800`; HLS stayed `running`, the playlist
   advanced across two discontinuities per reset, and segments decoded at
   720x400 and 1280x800. See [§0](#0-post-fix-live-rerun). The paragraph above
   remains the historical pre-fix finding.
2. **QMP preflight cannot connect while the node holds the socket.** With the
   node running, `scripts/gate2/qemu-preflight.sh` reported:
   `QMP control socket ... exists but did not answer with a QMP greeting`.
   After the node stopped, the same preflight passed (`5 passed, 0 failed`,
   one VNC-demuxer warning). This is the documented one-QMP-client constraint,
   but the wrapper/README should say to run preflight before starting the node
   or to expect this failure.
3. **D-Bus initial-mode mismatch.** See finding 1; snapshot was unaffected,
   HLS was affected at startup.
4. **Audio unproven.** No audio device/backend and no `/dev/snd` in the guest.
5. **Mouse pointer unproven.** The guest was a text console, so a pointer move
   has no visible guest artifact. Keyboard input was proven instead.
6. **Controller/dashboard not exercised here.** An unrelated controller from
   another session is listening on the fixed auxiliary ports 7381 and 47996 and
   owns `/dev/video0` (Python PID 3924630, ffmpeg PID 3925616). Starting a second
   controller would interfere with that session; it was not started. Node HTTP
   routes were checked directly instead.
7. **Hard power-off/destroy untested.** `/power/stop` remains a QMP pause.
   Reset/on/off semantics were not redefined here.

## 10. Pass / fail / skip

| Area | Result | Evidence |
|---|---|---|
| Guest asset inventory | **PASS** | Alpine ISO, host kernel, Windows images, tool availability |
| Disposable guest boot from read-only ISO | **PASS** | Alpine 3.21 login; no writable disk |
| QMP screendump shows guest | **PASS** | 1280×800 PPMs, OCR boot/login |
| Manual QMP keyboard visible in guest | **PASS** | typed login + `echo`, screen and serial marker |
| Node QMP input visible in guest | **PASS** | `echo ozma-node-input` in snapshot/HLS |
| Node shared QMP + snapshot | **PASS** | one client, 1280×800 JPEG |
| Node HLS from guest | **PASS** (before reset) | `seg_00050.ts`, h264 1280×800, OCR |
| HLS across guest reset | **PASS (post-fix)** | 3 resets, `1280x800 -> 720x400 -> 1280x800`, two discontinuities per reset; see §0 |
| Snapshots during/after resolution change | **PASS (post-fix)** | valid 1280×800 and 720×400 JPEGs during the third reset |
| Manual QMP pause/resume/reset | **PASS** | frozen serial/screen, resume, reboot |
| Node power stop/on/reset | **PASS** | paused → running, marker after resume, reboot |
| D-Bus snapshot of guest | **PASS** | 1280×800 JPEG, login OCR |
| D-Bus HLS after boot | **PASS** | `seg_00024.ts`, h264 1280×800, OCR |
| D-Bus HLS during/after mode change | **FAIL (pre-fix), not rerun** | 640×480 vs 1280×800 dimension error; the post-fix run covered QMP resets only |
| Audio | **SKIP** | no audio device/backend; no `/dev/snd` |
| Mouse pointer | **SKIP** | text console has no pointer artifact |
| Controller/dashboard | **SKIP** | unrelated controller owns aux ports and `/dev/video0` |
| `check-node.sh` (QMP node, after restart) | **PASS** | 5 passed, 0 failed, 2 skipped |
| `qemu-preflight.sh` (node stopped) | **PASS** | 5 passed, 0 failed, 1 warning, 4 skipped |

## 11. Cleanup

| Item | PID / path | Final state |
|---|---|---|
| Main QEMU | PID from `qemu/qemu.pid` (3919800) | `quit`; exited |
| D-Bus QEMU | PID from `dbus-qemu/qemu.pid` (3933980) | `quit`; exited |
| Main node (run 1) | 3929266 | SIGINT, `Ozma node shutting down` |
| Main node (run 2) | from `node/node2.pid` | SIGINT, `Ozma node shutting down` |
| D-Bus node (run 1) | from `dbus-node/node.pid` | SIGINT, `Ozma node shutting down` |
| D-Bus node (run 2) | from `dbus-node/node2.pid` | SIGINT, `Ozma node shutting down` |
| Temp root | `/tmp/opencode/ozma-gate2-guest-20261124-144126` | removed |
| Helper | `/tmp/opencode/gate2-qmp-helper.py` | removed |
| Target preview copy | `target/gate2-guest-evidence` | removed |
| Ports 47321/47323/60321/47331/47333/60331 | — | no listeners after cleanup |

Final checks are in the cleanup section of the task transcript. No libvirt
domain was started, stopped, defined, edited, or destroyed. The Windows qcow2
and ISO in `/var/lib/libvirt/images` and `/home/matt/vms` were not opened for
write. No sudo or network download was used. No commit was made; this report is
the only repository file added by this validation.

## 12. Remaining Gate 2 blockers

Guest-visible display, keyboard input, pause/resume/reset, node snapshots, and
QMP/D-Bus HLS have now been demonstrated on a real Alpine guest. Gate 2 still
cannot be claimed end to end because:

* HLS resolution-change handling is fixed and the **QMP guest-reset path is
  now live-verified** (§0); the D-Bus startup/mode-change path has not been
  re-run live since the restart/append-list fix, so D-Bus HLS across a mode
  change remains unproven;
* audio was not produced or captured;
* mouse-pointer behavior was not guest-visible on a text console;
* the unmodified controller/dashboard was not run against this guest in this
  session (an unrelated controller owns its fixed auxiliary ports and capture
  device);
* WebRTC media and controller regression/dashboard browser runs remain
  outstanding from the earlier reports;
* hard power-off/destroy remains untested and `/power/stop` is a pause.

## 13. Resolution-change fix and live verification

The failure recorded in §6 and §9 (finding 1) is addressed in
`ozma-node/src/hls.rs`:

* dynamic `HlsConfig::width`/`height` (`None`) now restart ffmpeg on a frame
  size change: the old child's stdin is closed, the child is killed and
  awaited, and a new generation is spawned with the new `-s WxH` plus the same
  encoder, bitrate, segment duration, playlist size, and output paths;
* restart argv uses `-hls_flags delete_segments+append_list+discont_start`, so
  the playlist is appended with a discontinuity marker rather than truncated,
  and ffmpeg continues segment file numbering (verified with ffmpeg n9.1,
  including a `SIGKILL` mid-run);
* `FramePacer::rebase` restarts the wall-clock schedule one interval after the
  completed restart, so a slow restart cannot emit a catch-up burst;
* a failed restart is a typed `HlsError::Restart`, marks the pipeline `failed`,
  and never writes to the dead child;
* explicitly pinned dimensions remain a contract: a mismatched frame is a typed
  `HlsError::FrameDimensions` failure (no scaling).

Default tests use the fake source/spawner only:
`cargo test -p ozma-node --lib hls::` (34 passed, 1 ignored). The live QMP
guest rerun is now complete: three resets reproduced `1280x800 -> 720x400 ->
1280x800`, `stream.m3u8` advanced across two `#EXT-X-DISCONTINUITY` markers per
reset, old and new segments decoded at 720x400 and 1280x800, snapshots stayed
valid JPEGs at both sizes, and `/api/v1/connection` stayed `running` throughout
(§0). The D-Bus startup transition (`640x480 -> 1280x800`) still needs the same
live rerun; it was not repeated in this follow-up.
