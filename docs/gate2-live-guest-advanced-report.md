# Gate 2 live guest advanced report: ACPI, audio, D-Bus GL/Disable/secondary console

**Scope:** close the remaining guest-behaviour gaps on the disposable Alpine
Linux live ISO: ACPI/power semantics, guest-produced audio, and the QEMU D-Bus
display paths for GL (including DMA-BUF import), `Disable`, and a secondary
console.

**Result:** ACPI, guest audio, the D-Bus `Disable`/secondary-console paths, and
the GL `ScanoutDMABUF`/`UpdateDMABUF` capture path were exercised live. The
DMA-BUF importer is a follow-up in §5.2. The important qualifications are:

| Area | Result |
|---|---|
| ACPI `system_powerdown` with no guest handler | **PASS (ignored)** — QEMU emits `POWERDOWN`, guest stays running |
| ACPI after starting Alpine's busybox `acpid` | **PASS** — guest prints `reboot: Power down` and QEMU exits |
| Node `POST /power/off` | **PASS with guest caveat** — ignored without `acpid`, powers off with it |
| Node `POST /power/stop` | **PASS** — QMP `quit`; QEMU process exits |
| Guest-initiated `reboot` through node `/input/type` | **PASS** — `reboot: Restarting system`; QEMU stays alive |
| Guest-initiated `poweroff` through node `/input/type` and manual QMP | **PASS** — `reboot: Power down`; QEMU exits |
| Node `/input/type` full ASCII punctuation string (+ tab probe) | **PASS after fix (follow-up)** — exact body echoed to the serial console, `0x09` tab captured, no stuck key; see §7.1 |
| Node `/input/type` selectable US/UK/DE/FR layouts | **PASS (follow-up)** — exact serial echoes for all four guest keymaps, AltGr/Shift released, dead-key and unsupported characters typed 400 atomically; see §7.2 |
| Guest audio | **PASS** — 440.1 Hz tone captured from QEMU's host audio monitor; peak −6.94 dBFS, tone RMS −9.96 dBFS |
| Node `/audio/*` routing | **PASS** — 523 Hz reference played to the null sink and captured; peak −26.89 dBFS |
| D-Bus 2D capture | **PASS** — BGRA `Scanout`/`Update`, snapshot and HLS |
| D-Bus `Disable` | **PASS on the GL path only** — QEMU emitted two `Disable` messages on reset; 2D VGA emitted none |
| D-Bus GL capture | **PASS (follow-up)** — `ScanoutDMABUF`/`ScanoutDMABUF2`/`UpdateDMABUF` importer maps linear packed and multi-plane YUV DMA-BUFs, issues `DMA_BUF_IOCTL_SYNC`, applies damage-only updates, and holds the initial all-black GL scanout briefly; live 1280×800 HLS, snapshot, v2 scanout, `UpdateDMABUF`, and reset `Disable` verified. Tiled/compressed modifiers remain typed unsupported |
| Secondary `Console_1` selection + live Scanout | **PASS with a second VGA device**; a guest-programmed second virtio-gpu head was **not** achieved |

> No `sudo`, no package download, no libvirt mutation, and no write to an
> original image were used. The Alpine ISO was attached read-only. Guest state
> lived in tmpfs. A small static playback helper was compiled locally from
> 222 lines of C using only kernel UAPI headers; no prebuilt binary and no
> download were used. The ISO hash was unchanged. See [Cleanup](#8-cleanup).

## 1. Run metadata

| Item | Value |
|---|---|
| Run | 2026-11-24 17:36–18:03 AEDT (2026-11-24T06:36–07:03Z) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD at finish | `e600c43630185d92df29099eff1931c65b61c0d7` (`e600c43`) |
| Working tree | Dirty from concurrent sessions; this run added the D-Bus fix, this report, short follow-up notes in existing reports, the `/input/type` key-hold fix (§7.1), and the selectable keyboard-layout change (§7.2) |
| Layout follow-up (§7.2) | 2026-11-24 ~19:15–19:35 AEDT; HEAD `9f0edc2` plus the uncommitted layout change; temp root `/tmp/opencode/ozma-kbd-layout-20261124` (removed) |
| Build | `cargo build -p ozma-node` → exit 0; `cargo test -p ozma-capture --lib dbus_display` → 26 passed, 1 ignored |
| Node binary | `target/debug/ozma-node`, 283,021,464 bytes, `ozma-node 0.1.0` |
| QEMU | `qemu-system-x86_64` 11.1.3, KVM (`accel=kvm:tcg`) |
| Guest | Alpine Linux 3.21 live, kernel `6.12.13-0-virt`, root in tmpfs |
| ISO | `/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso` |
| ISO SHA-256 | `f28171c35bbf623aa3cbaec4b8b29297f13095b892c1a283b15970f7eb490f2d` (unchanged) |
| Temp root | `/tmp/opencode/ozma-gate2-adv-20261124-173623` (removed) |
| Node HTTP ports | 47441, 47451, 47461, 47471, 47481, 47482 |
| HID/UDP ports | 60441, 60451, 60461, 60471, 60481, 60482 |
| Audio sink | `ozma-gate2-adv` null sink, 48 kHz stereo, PulseAudio-on-PipeWire |

The working tree was shared with other agent sessions; `Cargo.toml`/`Cargo.lock`
and several unrelated crates changed during the run. The build above includes
those concurrent changes, but this report's code change is limited to
`ozma-capture/src/dbus_display.rs`.

## 2. Disposable guest and audio hardware

The kernel and initramfs were extracted read-only from the ISO. The read-only
ISO was the boot CD; a second read-only ISO carried the local playback helper.

```bash
ROOT=/tmp/opencode/ozma-gate2-adv-20261124-173623
ISO='/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso'
xorriso -osirrox on -indev "$ISO" \
  -extract /boot/vmlinuz-virt "$ROOT/qemu/vmlinuz-virt" \
  -extract /boot/initramfs-virt "$ROOT/qemu/initramfs-virt"

pactl load-module module-null-sink \
  sink_name=ozma-gate2-adv \
  sink_properties=device.description=ozma-gate2-adv \
  rate=48000 channels=2

qemu-system-x86_64 \
  -name ozma-gate2-adv \
  -machine pc,accel=kvm:tcg -cpu host -smp 2 -m 1024 \
  -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" \
  -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modules=loop,squashfs,sd-mod,usb-storage,iso9660,snd-hda-intel alpine_dev=cdrom" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -drive "file=$ROOT/helper.iso,media=cdrom,readonly=on" \
  -device VGA \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -device usb-tablet,bus=xhci.0 \
  -audiodev "pa,id=snd0,out.name=ozma-gate2-adv" \
  -device intel-hda \
  -device hda-duplex,audiodev=snd0 \
  -display none \
  -chardev "socket,id=ser0,path=$ROOT/qemu/serial.sock,server=on,wait=off" \
  -serial chardev:ser0 \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" \
  -daemonize
```

A small `serial_bridge.py` connected the QEMU serial socket to a FIFO and a log
so guest commands could be typed and captured without a VGA console. The node
was started after boot with the shared QMP client:

```bash
RUST_LOG=info,ozma_qmp=debug,ozma_input=debug ./target/debug/ozma-node \
  --name ozma-gate2-adv \
  --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 47441 \
  --hid-udp-port 60441 \
  --cap hid,video,audio \
  --hls --hls-dir "$ROOT/node/hls" \
  --hls-fps 5 --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg \
  --audio-router pulseaudio \
  --audio-sink ozma-gate2-adv \
  --audio-track-root "$ROOT/node/tracks"
```

`GET /api/v1/connection` reported `hls: "running"`, `hls_error: null`,
`audio: "running"`, `audio_error: null`, and `power.status: "running"`.

## 3. ACPI and power

### 3.1 Guest ACPI inventory

Alpine 3.21 live has busybox `acpid` and an OpenRC init script, but the service
is **not running** by default and there is no `/etc/acpi/events` directory:

```text
# busybox --list | grep -i acpi
acpid
# ls -l /etc/init.d/acpid /sbin/acpid
-rwxr-xr-x 1 root root 267 /etc/init.d/acpid
lrwxrwxrwx 1 root root  12 /sbin/acpid -> /bin/busybox
# rc-service acpid status
 * status: stopped
# ps w | grep -i acpi | grep -v grep
   38 root 0:00 [irq/9-acpi]
# ls -l /etc/acpi/events
ls: /etc/acpi/events: No such file or directory
# ls -l /etc/acpi
drwxr-xr-x 2 root root 60 /etc/acpi/PWRF
```

`rc-update show` did not list `acpid`; the live image only runs it when started
manually. `/proc/acpi/events` does not exist.

### 3.2 Manual QMP `system_powerdown`

With the guest running and `acpid` stopped:

```text
<- {"execute":"system_powerdown"}
-> {"return":{}}
-> {"event":"POWERDOWN","timestamp":{"seconds":1795502427,"microseconds":348469}}
<- {"execute":"query-status"}   # 6 s later
-> {"return":{"status":"running","running":true}}
```

The guest serial log did not gain a shutdown line and the guest stayed at its
shell. This is the expected outcome: QEMU delivered the ACPI power-button
event, but no guest userspace handler acted on it.

After starting the busybox handler inside the guest:

```text
# rc-service acpid start
 * Starting busybox acpid ... [ ok ]
# rc-service acpid status
 * status: started
```

the same QMP `system_powerdown` produced:

```text
[  177.444700] reboot: Power down
```

and QEMU exited; the QMP socket disappeared.

### 3.3 Node power routes

| Step | Command / response | Guest / process evidence |
|---|---|---|
| `/power/off` without `acpid` | `POST /power/off` → `{"action":"off","ok":true}` | 6 s later `/power/state` → `{"available":true,"status":"running"}`; no serial shutdown |
| `/power/off` with `acpid` running | `POST /power/off` → `{"action":"off","ok":true}` | serial `reboot: Power down`; QEMU exited |
| `/power/stop` | `POST /power/stop` → `{"action":"force-off","ok":true}` | QEMU PID 102156 exited; `qmp.sock` removed |
| node `/input/type` `reboot\n` | `{"events":14,"ok":true}` | serial `[22.490141] reboot: Restarting system`; QEMU stayed alive; `/power/state` stayed `running` |
| node `/input/type` `poweroff\n` | `{"events":18,"ok":true}` | serial `[97.386434] reboot: Power down`; QEMU exited |

The node's `/power/stop` is no longer a pause: `ozma-power/src/qmp.rs` calls QMP
`quit` for the force-off action. The live run confirmed the process exit and the
loss of the QMP socket, not just a `paused` state.

Manual QMP keyboard injection was also used as a control: `send-key` typed
`root` and `poweroff` on the VGA console; the guest printed `reboot: Power down`
and QEMU exited.

### 3.4 ACPI result

**PASS, with the guest caveat that Alpine live does not run `acpid` by
default.** The node cannot make an unhandled ACPI power button power off a
guest; it correctly sends `system_powerdown` and reports success when QEMU
accepts the request. A guest with `acpid` (or another ACPI handler) powers off.

## 4. Guest audio

### 4.1 Guest audio inventory

The emulated `intel-hda`/`hda-duplex` device was detected, and the kernel
modules loaded:

```text
# ls -l /dev/snd
crw-rw---- 1 root audio 116,0  controlC0
crw-rw---- 1 root audio 116,4  hwC0D0
crw-rw---- 1 root audio 116,24 pcmC0D0c
crw-rw---- 1 root audio 116,16 pcmC0D0p
crw-rw---- 1 root audio 116,33 timer

# lsmod | grep -i snd
snd_hda_codec_generic ... 1
snd_hda_intel ... 0
snd_intel_dspcfg ... 1 snd_hda_intel
snd_hda_codec ... 2 snd_hda_codec_generic,snd_hda_intel
snd_pcm ... 3 snd_hda_intel,snd_hda_codec,snd_hda_core
```

The live ISO does **not** contain `aplay`, `speaker-test`, `amixer`,
`pw-play`, `pacat`, or `parec`:

```text
# which aplay speaker-test pw-play pacat parec amixer beep
/usr/bin/beep
# apk info 2>/dev/null | grep -Ei "alsa|acpi|pulse|pipewire|audio"
(no output)
```

`beep` is a busybox applet, not an ALSA/HDA player; it was not used as evidence.
The available ISO `/apks` repository also has no `alsa-utils`/`alsa-lib`
package. The exact blocker for using native guest tools was therefore: **no
ALSA user-space player and no `alsa-utils` package on the live ISO**.

### 4.2 Local static playback helper

No prebuilt helper existed locally. A 222-line C program using only
`<sound/asound.h>` kernel ioctls was compiled statically on the host with
`gcc -static` (no network, no libasound). It:

1. opens `/dev/snd/controlC0` and sets `Master Playback Volume` and
   `Master Playback Switch` (and the capture controls) to maximum/unmuted;
2. opens `/dev/snd/pcmC0D0p`;
3. issues `SNDRV_PCM_IOCTL_HW_PARAMS`, `PREPARE`, and `WRITEI_FRAMES`;
4. plays a 440 Hz stereo sine and drains.

The binary (922,704 bytes, SHA-256 `d586e51d…f92961`) was placed in a small
ISO and attached as `/dev/sr1`, read-only:

```text
# mkdir -p /mnt/helper
# mount -t iso9660 /dev/sr1 /mnt/helper
# cp /mnt/helper/ozma-tone /tmp/ozma-tone
# chmod +x /tmp/ozma-tone
# /tmp/ozma-tone /dev/snd/pcmC0D0p 3 48000 440
set Master Playback Volume   -> 74 (count=2 type=2)
set Master Playback Switch   -> 1 (count=2 type=1)
set Capture Volume           -> 74 (count=2 type=2)
set Capture Switch           -> 1 (count=2 type=1)
hw_params ok: rate=48000 channels=2 period=32 buffer=1024
played 144000 frames at 48000 Hz (3 s, 440.0 Hz)
```

The default codec state was muted: `/proc/asound/card0/codec#0` showed
`Amp-Out vals: [0x80 0x80]` (mute bit set), so playback without the control
writes was silent. Unmuting to the codec maximum was required for this guest.

### 4.3 Host monitor capture

While the guest played, the host captured the null-sink monitor:

```bash
timeout 10 parec \
  --device=ozma-gate2-adv.monitor \
  --format=s16le --rate=48000 --channels=2 \
  --latency-msec=20 --raw \
  > "$ROOT/artifacts/guest-tone-unmute.raw"
```

`pactl list short sink-inputs` showed QEMU's audio stream on the
`ozma-gate2-adv` sink, so this was the emulated HDA output, not a host-side
reference track.

Analysis of the 9.98 s capture:

```text
silence_start: 0
silence_end:   1.040667
silence_start: 4.010667
silence_end:   9.98

full capture:  peak  -6.94 dBFS   RMS  -15.22 dBFS
tone segment:  peak  -6.94 dBFS   RMS   -9.96 dBFS
dominant FFT bin: 440.1 Hz
```

The non-silent tone segment was 2.97 s, matching the guest's 3 s playback. The
tone frequency, stereo channel count, and sample rate all match the helper's
request. This is real guest-produced audio captured from QEMU's audio backend.

### 4.4 Node audio router

The node's `/audio/*` router was exercised separately against the same null
sink. It controls an existing PulseAudio/PipeWire graph and does not capture the
guest; the distinction matters.

```text
POST /audio/apply {"profile_id":"gate2","sink":"ozma-gate2-adv","volume":80}
→ {"backend":"pulseaudio","ok":true,"profile_id":"gate2","sink":"ozma-gate2-adv"}

POST /audio/play {"track":"ref-tone.wav","sink":"ozma-gate2-adv"}
→ {"backend":"pulseaudio","ok":true,"sink":"ozma-gate2-adv","track":"ref-tone.wav"}

POST /audio/stop
→ {"backend":"pulseaudio","ok":true}
```

The reference track was a locally generated 3 s, 523 Hz stereo WAV. Monitor
capture of the node playback:

```text
duration 5.98 s
full capture:  peak -26.89 dBFS   RMS -32.90 dBFS
best 1 s:      RMS  -29.90 dBFS   dominant 523.0 Hz
```

`GET /audio/nodes` remains a typed 501 (`audio-nodes-unsupported`), which is the
documented contract: the Rust router does not enumerate graph nodes. No nodes
were fabricated.

### 4.5 Audio result

**PASS for the emulated HDA path with a locally compiled helper and a guest
unmute.** **SKIP for native guest tools**: the Alpine live ISO has no
`aplay`/`speaker-test`/`pw-play`/`pacat`/`amixer`, and no local package
repository contains `alsa-utils`.

## 5. D-Bus GL, `Disable`, and secondary console

### 5.1 Host GL capability

The host has working EGL/GBM and virglrenderer:

```text
$ eglinfo
GBM platform:
  EGL client APIs: OpenGL OpenGL_ES
  OpenGL core profile version: 4.6 (Core Profile) Mesa 26.3.1-arch3.4
  OpenGL ES profile version: 3.2 Mesa 26.3.1-arch3.4
Surfaceless platform:
  EGL client APIs: OpenGL OpenGL_ES
  OpenGL core profile version: 4.6 ...
```

`libEGL.so`, `libgbm.so`, `libepoxy.so`, and `libvirglrenderer.so.1` are
present, and `/dev/dri/renderD128`–`renderD130` exist. QEMU 11.1.3 starts with
`-display dbus,gl=on` and `-device virtio-vga-gl`. A non-fatal Mesa warning
appeared:

```text
MESA: error: ZINK: failed to choose pdev
```

but QEMU continued and the guest booted, so this was not a blocker.

GL QEMU command (abridged):

```bash
qemu-system-x86_64 \
  -name ozma-dbus-gl -machine pc,accel=kvm:tcg -cpu host -smp 2 -m 1024 \
  -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modules=loop,squashfs,sd-mod,usb-storage,iso9660,virtio_gpu alpine_dev=cdrom" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -device virtio-vga-gl \
  -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
  -display "dbus,p2p=yes,gl=on" \
  -serial "file:$ROOT/dbus-gl/serial.log" \
  -qmp "unix:$ROOT/dbus-gl/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/dbus-gl/qemu.pid" -daemonize
```

The guest saw `virtio-vga`, `features: +virgl ...`, one scanout, and
`fb0: virtio_gpudrmfb frame buffer device`.

### 5.2 Live GL capture — DMABUF importer (follow-up)

#### 5.2.1 Original observation (was blocked)

The original run selected `Console_0`:

```text
D-Bus console id=dbus0 label="virtio-vga-gl" width=1280 height=800
D-Bus listener registered; awaiting Scanout
```

It then received, live:

```text
method="ScanoutDMABUF" body_bytes=36     (11 occurrences)
method="UpdateDMABUF"  body_bytes=16    (1724 occurrences)
method="MouseSet"      body_bytes=12     (1 occurrence)
```

It produced **no BGRA frame**, and no BGRA `Scanout` reached the source:

```text
hls: "failed"
hls_error: "HLS manifest ... has not been written yet"
GET /display/snapshot → 504 display.snapshot_timeout
QMP screendump → {"error":{"class":"GenericError","desc":"no surface"}}
```

The QMP screendump failure independently confirms that the GL display path has
no 2D surface to dump. The exact blocker was that QEMU's `-display dbus,gl=on`
uses `ScanoutDMABUF`/`UpdateDMABUF`, while the provider implemented only the
BGRA/pixman `Scanout`/`Update` members.

#### 5.2.2 Exact wire format (2026-11-24 follow-up)

A disposable QEMU 11.1.3 guest (`virtio-vga-gl`, `-display
"dbus,p2p=yes,gl=on"`, Alpine 3.21, read-only ISO) was probed with a raw
D-Bus/SCM_RIGHTS reader. The members are:

```text
ScanoutDMABUF(h dmabuf, u width, u height, u stride, u fourcc,
              t modifier, b y0_top)     signature "huuuutb", body 36 bytes
UpdateDMABUF(i x, i y, i w, i h)        signature "iiii",    body 16 bytes
```

`ScanoutDMABUF` body layout: `h` fd index at 0, width at 4, height at 8, stride
at 12, fourcc at 16, four D-Bus padding bytes at 20, `t` modifier at 24,
`b y0_top` at 32. The descriptor is attached with SCM_RIGHTS. The two initial
live scans were:

```text
1280x800 stride=5120 fourcc=0x34324258 'XB24' (XBGR8888) modifier=0 y0_top=0 fd=5
1280x800 stride=5120 fourcc=0x34324241 'AB24' (ABGR8888) modifier=0 y0_top=1 fd=7
```

`fstat` reported 4,096,000 bytes (`5120 × 800`), exactly the mapped size. `pread`
fails with `ESPIPE`; a read-only `mmap` succeeds. `UpdateDMABUF` carries **no
descriptor and no pixels**: it is a damage rectangle for the current scanout, and
the GPU has already written the mapped buffer. A 70 s probe saw 2
`ScanoutDMABUF` and 334 `UpdateDMABUF` messages. `MouseSet` (`iii`) remains
informational and is not a frame.

#### 5.2.3 Importer implementation

New private module `ozma-capture/src/dbus_dmabuf.rs`, plus a
`DBusTransport::read_with_fds`/`read_message_with_fds` path in
`ozma-capture/src/dbus_display.rs`:

1. **Descriptor reception.** The listener reads the first byte of every message
   with `recvmsg(MSG_CMSG_CLOEXEC)`, adopts `SCM_RIGHTS` descriptors as
   `OwnedFd`, and requires the header's `UNIX_FDS` count to match. The `h` index
   selects the descriptor; any unused descriptors are dropped and closed.
2. **Validation.** Body length/signature, non-zero and configured-bounded
   dimensions, stride, checked `stride × height`, and the descriptor's reported
   size are validated before touching pixels. A descriptor that is too small is
   a typed geometry error.
3. **Modifier.** Only `DRM_FORMAT_MOD_LINEAR` (`0`) is mapped. Any tiled or
   compressed modifier, and any unknown fourcc, returns
   `DBusError::Unsupported` naming the modifier and fourcc. No fake frame is
   emitted.
4. **Formats.** Packed 32-bit scanouts are converted to BGRA:
   `XBGR8888`/`ABGR8888` (memory R,G,B,A; swaps R/B), `XRGB8888`/`ARGB8888`
   (memory B,G,R,A; direct), `BGRA8888`/`RGBA8888` (DRM word order, shuffled),
   and single-fd contiguous `NV12` (limited-range BT.601, Y plane then UV
   plane). `y0_top=0` is flipped vertically. `X` formats become opaque BGRA.
5. **Mapping and lifetime.** `memmap2` maps the descriptor read-only; the
   `File` (sole owner of the descriptor) and mapping live in the imported
   surface, with the mapping dropped before the descriptor. `Scanout` (pixman),
   `Disable`, and source drop all release the previous mapping. `UpdateDMABUF`
   re-snapshots the full mapped buffer into an owned BGRA frame.
6. **Unsafe isolation.** The crate previously used `#![forbid(unsafe_code)]`.
   DMA-BUF has no safe read path (`pread` returns `ESPIPE`), so the crate now
   uses `#![deny(unsafe_code)]` with `#[allow(unsafe_code)]` only on
   `dbus_dmabuf`. The unsafe operations are `memmap2`'s read-only mapping
   constructor, justified by the fixed lifetime/size of a DMA-BUF export, and
   `OwnedFd::from_raw_fd` adoption for descriptors just received via
   `SCM_RIGHTS`; both are confined to that module. The rest of
   `ozma-capture` remains unsafe-free.

#### 5.2.4 Live verification

Follow-up run: 2026-11-24 18:29–18:50 AEDT, QEMU 11.1.3, Alpine 3.21 live
(read-only ISO SHA-256 `f28171c3…490f2d`), temp root
`/tmp/opencode/ozma-dmabuf` (removed), node HTTP port 47591. HEAD was `f12add9`
plus the uncommitted DMABUF change and concurrent-session changes. The node was
built from this tree and started with `--capture-provider dbus --dbus-console 0
--dbus-width 1280 --dbus-height 800 --hls`.

- `cargo test -p ozma-capture --lib dbus_`: **40 passed, 0 failed, 1 ignored**.
- `OZMA_DBUS_TEST_QMP=… real_qemu_dbus_display_pushes_a_frame`: **PASS**,
  `1280x800`, `4,096,000` bytes.
- Node log: `D-Bus DMA-BUF scanout imported` for `fourcc=875709016` (`XB24`),
  modifier `0x0000000000000000`, `y0_top=false`, and `fourcc=875708993`
  (`AB24`), `y0_top=true`.
- HLS: `GET /api/v1/connection` reported `hls: "running"`, `hls_error: null`;
  `stream.m3u8` and 1 s segments (~132 KB) were written.
- A decoded HLS segment was **1280×800** and non-black (56,274 non-black
  pixels), with real console text. The one-shot `GET /display/snapshot` also
  returned a **1280×800 JPEG** (126,080 bytes) when the guest screen had
  content.
- **Channel order:** a known `printf` wrote red, green, then blue blocks to the
  guest console. In the HLS frame, exact ANSI colours appeared left-to-right in
  that order: red at x=2–31, green at x=34+, blue at x=66+ on the same row.
  Treating `ABGR8888` as BGRA would have swapped red and blue; the live frame
  proves the conversion is correct.
- **`y0_top=false`:** after `POST /power/reset` the new 1280×800 scanout had
  `y0_top=false`. A fresh red/green/blue line appeared at y=18–25 (top of the
  image), not at the bottom, so the vertical flip is active and correct.
- **`UpdateDMABUF`:** continuous console output (login, prompt, `printf`) changed
  the HLS frames; `UpdateDMABUF` produced new BGRA frames without a pixman
  `Scanout`.
- **`Disable`:** `POST /power/reset` produced `D-Bus Disable received;
  framebuffer cleared` (twice in the reset sequence), followed by fresh
  720×400 and 1280×800 scanouts. The DMABUF mapping and framebuffer were
  cleared and re-established, and HLS continued.

**Remaining GL limits:** non-linear/tiled/compressed modifiers are rejected with a
typed error naming the modifier and fourcc; they are not interpreted as linear
pixels. The 2026-11-24 v2 follow-up (see §5.7) added multi-plane
`ScanoutDMABUF2`, `DMA_BUF_IOCTL_SYNC`, damage-only `UpdateDMABUF` copies, and
an initial-black hold. Its live QEMU host exported only linear single-plane
buffers, so the multi-plane and tiled paths are unit-covered but not
live-exercised. The real-QEMU test remains opt-in and is not in CI.

### 5.3 `Disable` — live on GL, absent on 2D

| Trigger | Display path | QEMU emitted `Disable`? |
|---|---|---|
| `POST /power/reset` (`system_reset`) | `virtio-vga-gl` + `dbus,gl=on` | **YES — two `Disable` messages**; provider logged `D-Bus Disable received; framebuffer cleared` |
| `POST /power/reset` | `virtio-gpu-pci` + `dbus,p2p=yes` (2D) | **NO** — only a fresh listener registration and `Scanout` |
| `device_del {"id":"vga0"}` on `-device VGA,id=vga0` | 2D VGA | **NO** — QMP error `Device 'VGA' does not support hotplugging` |
| `device_del {"id":"gpu0"}` on `-device virtio-gpu-pci,id=gpu0` | 2D virtio-gpu | **NO** — QMP error `Device 'virtio-gpu-pci' does not support hotplugging` |

So `Disable` is **not** a dead code path: QEMU 11.1.3 really emits it on the GL
listener when the guest resets. The provider's handler clears the framebuffer
and emits no frame, as designed. On the BGRA 2D path, QEMU's
`dbus_dcl_ops` has no disable callback, so a 2D VGA capture never sees
`Disable` regardless of reset or hotplug attempt.

The provider also logs any unhandled method at debug level (for example
`MouseSet` and `CursorDefine`), which is how the GL method names were first
observed instead of silently disappearing.

### 5.4 Secondary `Console_1`

Two configurations were tested.

**A. `virtio-gpu-pci,max_outputs=2` (2D):** `Console_1` selection worked and
reported `label="virtio-gpu-pci.1"`, but the guest saw only the first virtio
output:

```text
/sys/class/drm/card0-Virtual-1/status connected
/sys/class/drm/card0-Virtual-2/status disconnected
/sys/class/drm/card0-Virtual-1/enabled enabled
/sys/class/drm/card0-Virtual-2/enabled disabled
/proc/fb: 0 virtio_gpudrmfb
```

QEMU sent no `Scanout` for `Console_1`; HLS stayed failed. Starting the node's
`Console_1` listener **before** resuming the guest did not change this.

This configuration also exposed a secondary-console bug: the console
connection received a 1280×800 `PropertiesChanged` (matching `Console_0`'s
geometry) while `Console_1` was 640×480, and the source applied it to
`Console_1`. The selected console must only accept signals whose path matches
its own `Console_N`. That is fixed in this run; see §5.5.

**B. Two display devices — `virtio-gpu-pci` plus `VGA`:** `Console_1` resolved
to the second device and delivered a live BGRA surface:

```text
QEMU D-Bus console id=dbus0 label="VGA" width=720 height=400
D-Bus console size signal received id=dbus0 width=720 height=400
D-Bus console geometry changed; resizing Scanout id=dbus0 width=720 height=400
GET /display/snapshot → 200, JPEG 720x400, 108,935 bytes
HLS: running; segment 720x400, duration 1.000000
```

The guest saw both devices:

```text
/sys/class/drm/card0-Virtual-1/status connected   (virtio-gpu)
/sys/class/drm/card1-Virtual-2/status connected   (bochs-drm / VGA)
/sys/class/drm/card1-Virtual-2/enabled disabled
/proc/fb: 0 virtio_gpudrmfb, 1 bochs-drmdrmfb
```

**Honest limitation:** the `Console_1` snapshot showed early Alpine kernel boot
text, not the live login prompt. The guest's framebuffer console remained on
fb0 (virtio-gpu); the second VGA's surface was QEMU-device content and did not
track later console output. Adding `fbcon=map:1 video=Virtual-2:720x400` did not
change that. This is a live secondary-console `Scanout` from a second emulated
display device with a running guest, but it is **not** a guest-programmed second
virtio-gpu head. Lighting a second virtio head needs a guest DRM/modesetting
client (there is no `modetest`/`libdrm` tool on the ISO) or a kernel/fbcon
mapping that the ISO did not honour.

### 5.5 Code change

`ozma-capture/src/dbus_display.rs`:

1. **Console signal path filter.** `console_signal_applies(path, selected)`
   accepts a `PropertiesChanged` only when its D-Bus object path matches the
   configured `Console_N`; a pathless signal is still accepted for older QEMU
   compatibility. This prevents `Console_0` geometry from resizing a
   `Console_1` capture.
2. **Unhandled-method diagnostics.** A method call other than
   `Scanout`/`Update`/`Disable` now logs `method` and `body_bytes` at debug
   level. This is observability only; the capture surface is unchanged.

New test:
`console_properties_changed_only_applies_to_the_selected_console`.

```text
$ cargo test -p ozma-capture --lib dbus_
test result: ok. 40 passed; 0 failed; 1 ignored; 0 measured; 103 filtered out
```

### 5.6 DMABUF follow-up code change

Files changed in the follow-up:

- `ozma-capture/src/dbus_display.rs` — SCM_RIGHTS reception
  (`DBusTransport::read_with_fds`, `read_message_with_fds`), the
  `ScanoutDMABUF`/`UpdateDMABUF` match arms, DMABUF frame snapshots, and the
  `Disable`/pixman reset of the mapped surface.
- `ozma-capture/src/dbus_dmabuf.rs` (new) — body parsing, fourcc/modifier
  validation, read-only `memmap2` mapping, BGRA conversion, and typed errors.
- `ozma-capture/src/lib.rs` — `#![forbid(unsafe_code)]` narrowed to
  `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` only on
  `dbus_dmabuf`.
- `ozma-capture/Cargo.toml` — `memmap2` dependency and the `nix` `fs` feature
  for the descriptor-close tests.

Tests added: live-layout parsing, ABGR/XRGB conversion, bottom-up row flip,
NV12 conversion, tiled-modifier rejection, unknown-fourcc/geometry rejection,
fd close on success/failure, adopted-fd cleanup, scanout+update end-to-end,
missing-fd protocol error, unsupported-modifier typed error, and `Disable`
clearing a mapped surface. The pixman `Scanout`/`Update` tests are unchanged
and still pass.

```text
$ cargo test -p ozma-capture
test result: ok. 140 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

No `ozma-power`/ACPI change was needed. `POST /power/stop` already calls QMP
`quit`; the live process-exit test confirms the documented force-off semantics.

### 5.7 DMABUF v2, sync, damage, and initial-black follow-up (2026-11-24)

**Scope.** Extend the GL importer beyond the v1 single-plane message:

- Advertise `org.qemu.Display1.Listener.Unix.ScanoutDMABUF2` in the listener's
  `Interfaces` property. QEMU 11.1.3 then uses `ScanoutDMABUF2` for every
  DMA-BUF scanout (`ui/dbus-listener.c`), while the v1 `ScanoutDMABUF` arm
  remains for older peers.
- Parse the variable-size `ahuuuuauauuuuutb` body: `ah` fd indices, `x/y` origin,
  `width/height`, per-plane `au` offsets and strides, `num_planes`, fourcc,
  backing geometry, modifier, and `y0_top`. The parser validates array lengths,
  plane count (`1..=4`), fd/offset/stride equality, D-Bus alignment, and rejects
  trailing non-zero bytes.
- Import each plane into its own read-only `memmap2` mapping with checked
  `offset + stride × rows` bounds and a per-plane minimum row size. Supported
  linear layouts are packed XBGR/ABGR/XRGB/ARGB/BGRA/RGBA, two-plane
  NV12/NV21, three-plane YUV420/YVU420, and v1 single-fd contiguous NV12.
  A tiled/compressed modifier is still a typed unsupported error naming the
  modifier and fourcc.
- Issue `DMA_BUF_IOCTL_SYNC` start/end read around every snapshot and damage
  copy. `ENOTTY`/`EINVAL`/`EOPNOTSUPP` are treated as a coherent exporter
  (needed for the tempfile unit tests); any other sync failure is a typed
  error. Live QEMU produced no “sync is not supported” diagnostic, so the real
  DMA-BUFs accepted the ioctl.
- Keep an owned BGRA frame for the mapped scanout. `UpdateDMABUF` now copies
  only the damaged rectangle into that frame instead of re-snapshotting the
  whole buffer, including the `y0_top` row flip and multi-plane chroma
  addressing. A stale damage rectangle larger than the current scanout (QEMU
  sends one during a mode change) is logged and ignored until the next
  `ScanoutDMABUF`/`ScanoutDMABUF2`, instead of failing HLS.
- Hold an all-black first scanout for at most 750 ms. QEMU frequently sends the
  initial GL texture before the guest has drawn; if the first damage makes the
  frame non-black it is emitted immediately. A genuinely black screen is
  served after the grace period, so this is a delay rather than a permanent
  filter.

**Live run.** 2026-11-24 19:17–19:30 AEDT, QEMU 11.1.3, Alpine 3.21 live
(read-only ISO, unchanged), `virtio-vga-gl`, `-display dbus,p2p=yes,gl=on`,
temp root `/tmp/opencode/ozma-dmabuf2-20261124-191747` (removed), node HTTP
port 47851. The node ran with `--capture-provider dbus --dbus-console 0
--dbus-width 1280 --dbus-height 800 --hls`.

- The ignored real-QEMU test passed: `1280x800`, `4,096,000` bytes.
- Node debug logs show `D-Bus multi-plane DMA-BUF scanout imported` with
  `planes=1`, `x=0`, `y=0`, backing `1280x800`, fourcc `875709016` (`XB24`)
  `y0_top=false`, then fourcc `875708993` (`AB24`) `y0_top=true`, both with
  modifier `0x0000000000000000`. This proves the live v2 message is parsed and
  imported, not a v1 fallback.
- The first scanout was all black and logged as held; the following non-black
  scanout started HLS. A snapshot taken during a later reset returned a
  720×400 frame with 4,246 non-black samples, not an all-black JPEG.
- `DMA_BUF_IOCTL_SYNC` produced no “not supported” diagnostic; capture and HLS
  continued, so the real exporter accepted the sync ioctl.
- HLS reported `hls: "running"`, `hls_error: null`; segments decoded as
  `1280x800 yuv420p` and were non-black. `GET /display/snapshot` returned a
  1280×800 JPEG (about 126 KB).
- A guest `printf` wrote red, green, and blue background blocks. In both the
  snapshot and a decoded HLS segment the blocks appeared left-to-right as
  red x=0–15, green x=16–31, blue x=32–47 at y=752–766, confirming the channel
  order and the `y0_top` flip direction on the live scanout.
- `POST /power/reset` produced `D-Bus Disable received; framebuffer cleared`,
  then fresh 1280×800 and 720×400 scanouts and (during the transition) a stale
  `UpdateDMABUF` rectangle of 1280×800 against the 720×400 scanout. The stale
  update was ignored, HLS stayed `running`, and the next scanout restored
  frames. A second reset reproduced the same recovery.

**Multi-plane/tiled live limit.** The host renderer exported only linear,
single-plane buffers for the GL console (`planes=1`, modifier `0`), so a live
multi-plane scanout was not available and no tiled modifier was observed. The
multi-plane conversion and fd/offset/stride validation are covered by unit
tests with separate luma/chroma fds, offsets, and strides. Importing a tiled or
compressed modifier would require a real GBM/EGL importer (EGL display + GBM
buffer import + GL readback). This tree has no `libgbm`/EGL Rust bindings, and
doing it with raw FFI would be unsafe-heavy; the exact blocker is therefore the
absence of both a tiled live source and a safe importer crate. Tiled input is
still refused with a typed error rather than misread.

**Tests.** `cargo test -p ozma-capture --lib dbus_`: **54 passed, 0 failed, 1
ignored**. The full `cargo test -p ozma-capture` run passed too. New tests cover
v2 body parsing (including array/plane mismatches), two-plane NV12 with
offsets/strides, packed sub-rectangles, damage-only copies, tiled-modifier
rejection, v2 fd-count protocol errors, initial-black hold, stale oversized
damage recovery, and descriptor ownership. The fd-cleanup tests compare the
descriptor's `/proc/self/fd` inode instead of closing a raw number, avoiding
cross-test fd-reuse corruption under the parallel runner.

## 6. Pass / fail / skip

| Area | Result | Evidence |
|---|---|---|
| Alpine live boot from read-only ISO | **PASS** | login prompt; no writable disk |
| Audio hardware (`intel-hda`/`hda-duplex`, `-audiodev pa`) | **PASS** | guest `/dev/snd/pcmC0D0p`, modules loaded |
| Guest tools `aplay`/`speaker-test`/`pw-play`/`pacat`/`amixer` | **SKIP / BLOCKED** | none present; busybox `beep` only |
| Guest audio playback + host monitor capture | **PASS** | 440.1 Hz; peak −6.94 dBFS; tone RMS −9.96 dBFS |
| Node `/audio/apply` + `/audio/play` + monitor capture | **PASS** | 523.0 Hz; peak −26.89 dBFS |
| ACPI `system_powerdown` without guest handler | **PASS (ignored)** | `POWERDOWN` event; guest running 6 s later |
| ACPI after busybox `acpid` start | **PASS** | `reboot: Power down`; QEMU exited |
| Node `/power/off` without `acpid` | **PASS (ignored)** | `ok:true`; state `running` |
| Node `/power/off` with `acpid` | **PASS** | guest power down; QEMU exited |
| Node `/power/stop` hard quit | **PASS** | QEMU PID 102156 exited; QMP socket gone |
| Guest-initiated `reboot` via node input | **PASS** | `reboot: Restarting system`; QEMU alive |
| Guest-initiated `poweroff` via node input and manual QMP | **PASS** | `reboot: Power down`; QEMU exited |
| D-Bus 2D `Scanout`/`Update`, snapshot, HLS | **PASS** | 1280×800 JPEG; HLS `running` |
| D-Bus GL `ScanoutDMABUF` + `UpdateDMABUF` | **PASS (follow-up)** | live 1280×800 HLS (56,274 non-black px); snapshot JPEG 126,080 bytes; correct R/G/B order; `y0_top=false` flip verified |
| D-Bus GL capture before the follow-up | **FAIL / BLOCKED (historical)** | 11 `ScanoutDMABUF`, 1724 `UpdateDMABUF`, zero BGRA frames |
| D-Bus GL tiled/compressed modifier | **TYPED UNSUPPORTED** | `DBusError::Unsupported` names modifier + fourcc; no fake frame |
| D-Bus `Disable` on GL reset | **PASS** | two `Disable` messages; handler cleared framebuffer and DMABUF mapping |
| D-Bus `Disable` on 2D reset/hotplug | **ABSENT, as expected** | no `Disable`; device_del not hotpluggable |
| `Console_1` selection | **PASS** | `virtio-gpu-pci.1` / `VGA` labels |
| `Console_1` live BGRA Scanout | **PASS with second VGA** | 720×400 JPEG + HLS; content early boot text |
| Guest-programmed second virtio-gpu head | **BLOCKED** | guest `Virtual-2` disconnected/disabled; no modesetting tool |
| Node `/input/type` shifted characters | **PASS after fix (follow-up)** | full punctuation string reached the guest exactly; tab captured as `0x09`; no stuck key (§7.1) |

## 7. Findings and remaining blockers

1. **ACPI is a guest-userspace contract, not a node bug.** QEMU always emits
   `POWERDOWN`; Alpine live does not run `acpid` by default. `POST /power/off`
   is honest but cannot force an unhandled guest to power off. `POST /power/stop`
   is the hard path and does exit QEMU.
2. **Guest audio needs a guest player and an unmute.** The live ISO has no
   ALSA tools, and the emulated HDA codec starts muted (`Amp-Out 0x80`). The
   local static helper proved the QEMU/HDA/host-monitor path end to end, but it
   is a test artifact, not a guest package. Native `aplay`/`pw-play` evidence
   remains blocked by the ISO contents.
3. **D-Bus GL is now a capture surface for linear DMABUFs.** The GL listener
   emits `ScanoutDMABUF`/`UpdateDMABUF`; the provider now receives the
   SCM_RIGHTS descriptor, maps linear XBGR/ABGR/XRGB/ARGB/BGRA/RGBA (and
   single-fd contiguous NV12), flips `y0_top=false`, and re-snapshots on
   `UpdateDMABUF`. Live 1280×800 HLS, snapshot, update, channel order, and
   `Disable` on reset are verified. Remaining gaps are non-linear/tiled
   modifiers (typed unsupported), multi-plane `ScanoutDMABUF2`, explicit
   DMA-BUF sync on non-coherent hosts, and a damage-only update fast path. A
   one-shot snapshot can still catch the initial black scanout; continuous HLS
   is the reliable live path.
4. **Secondary console is live but not a guest second head.** `Console_1`
   selection and a live `Scanout` were demonstrated with a second VGA device.
   A `virtio-gpu-pci,max_outputs=2` guest kept `Virtual-2` disconnected and
   produced no second Scanout. The second VGA's captured content was stale
   early boot text because fbcon stayed on fb0.
5. **Secondary geometry signal bug fixed.** `Console_0`'s 1280×800
   `PropertiesChanged` was applied to `Console_1` (640×480). The path filter
   and its regression test are in `ozma-capture/src/dbus_display.rs`.
6. **Node shifted-character input anomaly root-caused and fixed.** The node
   submitted each key-down and key-up back-to-back and only paced *between*
   characters. QEMU's emulated USB keyboard reports its current state at the
   guest's poll cadence, so a press/release pair could collapse into one
   released report (the character was lost) or lose the release (the key
   auto-repeated). The earlier run's frozen line and non-responsive input were
   this stuck-key state, not a hung HTTP handler: QMP had accepted every event.
   The fix in `ozma-node/src/controller_compat.rs` holds each non-modifier key
   for 20 ms, keeps the 15 ms inter-character delay, sends explicit left-Shift
   and right-Alt (AltGr) down/up around the character, releases each key and
   modifier in reverse order, releases any held state on a submit error, and
   rejects unsupported text before sending the first event. See §7.1 for the
   live re-run and §7.2 for selectable layouts.
7. **Not in this run:** WebRTC media, controller/dashboard, mouse-pointer
   artifacts (text console), and non-linear/multi-plane DMA-BUF capture. D-Bus
   GL with linear DMA-BUFs is covered by the follow-up in §5.2.

### 7.1 Follow-up: `/input/type` full ASCII punctuation (2026-11-24)

After the fix, a fresh disposable Alpine 3.21 guest (same read-only ISO and
QEMU command as §2, temp root
`/tmp/opencode/ozma-input-fix-fixed-20261124-183252`, node HTTP port 47522,
HID port 60522) was logged in with `/input/type` `root\n`, then given this
96-character here-doc through the same route. The follow-up ran on HEAD
`f12add9` with the uncommitted `ozma-node/src/controller_compat.rs` change in
the working tree.

```text
cat > /dev/ttyS0 <<'Z'
azAZ09 _ > | ~ : " < ? + = - , . / ; ' \ [ ] ` { } ! @ # $ % ^ & * ( )
Z
```

The request returned `{"events":250,"ok":true}` in 3.58 s, and the guest's
serial console received the body byte-for-byte:

```text
localhost login: azAZ09 _ > | ~ : " < ? + = - , . / ; ' \ [ ] ` { } ! @ # $ % ^ & * ( )
FIXED-PUNCTUATION-OK
```

A following `echo FIXED-PUNCTUATION-OK > /dev/ttyS0` returned
`{"events":118,"ok":true}` and executed, so no modifier or key was left down.
The node log showed every batch sent on the shared QMP client; there was no
HTTP timeout or queue stall. A separate `dd bs=1 count=4 of=/dev/ttyS0` read
the bytes `A<TAB>B\n` and the serial log contained `41 09 42 0d 0a`, confirming
the Tab mapping (`0x2B`) itself. (Typing a literal Tab inside the interactive
shell's own line editor is consumed for completion; the `dd` test bypasses
that guest feature, not the node mapping.)

Before the fix, the same punctuation sequence produced a corrupted line and
then an endless `5` auto-repeat; the here-doc terminator was never accepted and
serial received no body. That pre-fix guest was discarded without being used as
PASS evidence.

**Keyboard-layout follow-up:** this run was US-only. §7.2 adds selectable
`us`/`uk`/`de`/`fr` layouts and live evidence for all four. Dead-key sequences,
compose, and IME remain unsupported and are typed 400 before any event. Typing
is still paced at a fixed 20 ms key hold plus 15 ms between characters, not by
guest acknowledgement; the 4096-character limit therefore takes about
2.5 minutes, and an unusually slow guest may still need a larger hold or a
guest-side keymap check.

### 7.2 Follow-up: selectable keyboard layouts (2026-11-24)

`POST /input/type` now takes `--keyboard-layout us|uk|de|fr`
(`OZMA_KEYBOARD_LAYOUT`, default `us`). The node maps each character to the
physical HID usage plus the Shift/AltGr state that produces it on the selected
guest console keymap. The complete request is validated before the sink is
touched, so a dead-key-only or non-ASCII character anywhere in the text produces
a typed 400 without typing anything.

This follow-up ran on HEAD `9f0edc2` with the uncommitted layout change, a fresh
disposable Alpine 3.21 guest, QEMU 11.1.3, `-device usb-kbd`, and the QMP input
provider. The guest keymaps came from the ISO's
`kbd-bkeymaps-2.6.4-r2.apk`, extracted under `/tmp` and loaded with busybox
`loadkmap`:

```text
zcat /tmp/bmaps/usr/share/bkeymaps/de/de.bmap.gz > /tmp/de.bmap
loadkmap < /tmp/de.bmap
zcat /tmp/bmaps/usr/share/bkeymaps/gb/gb.bmap.gz > /tmp/gb.bmap
loadkmap < /tmp/gb.bmap
zcat /tmp/bmaps/usr/share/bkeymaps/fr/fr-latin9_nodeadkeys.bmap.gz > /tmp/fr.bmap
loadkmap < /tmp/fr.bmap
```

A node was restarted with the matching `--keyboard-layout` for each keymap.
Each test used a here-doc that the guest wrote to `/dev/ttyS0`, so the serial log
shows the guest's interpretation of the injected usages, not the request body.
All four echoes matched exactly:

```text
US azAZ09 _ > | ~ : " < ? + = - , . / ; ' \ [ ] ` { } ! @ # $ % ^ & * ( )
US-LAYOUT-OK
UK azAZ09 " @ # ~ £ ¬ \ | < > = + - _ ; : ' , . / ? ! $ % ^ & * ( )
UK-LAYOUT-OK
DE azAZ09 äöüß ÄÖÜ @ € {[]} \ | < > = + - _ # ' ? ; : , .
DE-LAYOUT-OK
DE-NO-STUCK
FR azAZ09 aqzwm éèàçù @ # ~ ^ ` {[]} \ | < > 1&2é 9ç 0à
FR-LAYOUT-OK
```

The requests returned `{"ok":true}` with 346 (US), 332 (UK), 316 (DE), and 312
(FR) events. The `DE-NO-STUCK` marker was typed after an AltGr sequence and
executed, and every marker needed a working Shift release, so no modifier was
left held.

Atomic rejection was checked against the German keymap. `ab^` (German `^` is
`dead_circumflex`) returned HTTP 400 `{"code":"dead-key-required"}` and `abé`
(no direct German key) returned HTTP 400
`{"code":"unsupported-character"}`; the serial log's byte count was unchanged
(28175 before and after both requests), so no partial text reached the guest.

**French keymap variance:** plain `fr-latin9` was tried first. Its AltGr+7 is
`dead_grave`, so the backtick consumed the following space and the serial line
became ``... ^ `{[]} ...`` instead of ``... ^ ` {[]} ...``. The FR table therefore
targets `fr-latin9_nodeadkeys` (and Alpine's matching bmap), which produced the
exact line above. The layout is configured, not detected; a guest with plain
`fr-latin9` should use that keymap's nodeadkeys variant or send raw
`/input/key` events for the grave key.

**Remaining layout gaps:** dead-key sequences, compose, and IME are not
implemented. German `^`, `~`, and the grave key are rejected as dead-key-only;
the French dead-key accents `¨`, `´`, and `¸` are rejected. Non-ASCII text with
no direct key on the selected layout (CJK, emoji, combining marks, and `é` on
US/UK/DE) is rejected. The node does not inspect the guest keymap, so
`--keyboard-layout` must match it. The ISO `#~` position (HID `0x32`) needed a
new mapping to `KEY_BACKSLASH`/QEMU `backslash`; it is covered by
`cargo test -p ozma-input --lib keymap` and `cargo test -p ozma-qmp`.

## 8. Cleanup

All QEMU, node, ffmpeg, serial-bridge, and audio-capture processes were stopped;
the null sink was unloaded; the temp root and helper sockets/files were removed.
The final cleanup table is filled after the run:

| Item | PID / path | Final state |
|---|---|---|
| Audio/power QEMU + node | audio QEMU pidfile `qemu/qemu.pid` | exited during the ACPI tests |
| D-Bus GL QEMU | 104716 | `quit`; exited |
| D-Bus GL node | 122766 | SIGINT; exited |
| D-Bus second-console QEMU | 131751 | `quit`; exited |
| D-Bus second-console node + ffmpeg | 137657 / 137673 | SIGINT; exited |
| D-Bus DMABUF follow-up QEMU | 238742 | node `/power/stop` (QMP `quit`); exited |
| D-Bus DMABUF follow-up node | 292844 | SIGINT/TERM did not exit (QMP gone); SIGKILL; stopped |
| D-Bus DMABUF follow-up ffmpeg | 297129 | exited with the node |
| D-Bus DMABUF follow-up temp root | `/tmp/opencode/ozma-dmabuf` | removed |
| D-Bus DMABUF follow-up ports | 47591/60591 | no listeners |
| Keyboard-layout follow-up QEMU | 398049 (`qemu/qemu.pid`) | SIGTERM; exited |
| Keyboard-layout follow-up nodes | `node/node.pid` (restarted per layout) | SIGTERM; exited |
| Keyboard-layout follow-up temp root | `/tmp/opencode/ozma-kbd-layout-20261124` | removed |
| Keyboard-layout follow-up ports | 47741/60741 | no listeners |
| Serial bridges | 131746 and the per-run bridge PIDs | killed |
| Null sink | `ozma-gate2-adv` (module 536870916) | unloaded; `pactl list short sinks` no longer shows it |
| Temp root | `/tmp/opencode/ozma-gate2-adv-20261124-173623` | removed |
| Helper binary/source | `/tmp/opencode/ozma-gate2-adv-tone*` | removed |
| Helper scripts | `/tmp/opencode/ozma-qmp.py`, `/tmp/opencode/serial_bridge.py` | removed |
| Target evidence copy | `target/gate2-adv` | removed |
| Ports | 47441/47451/47461/47471/47481/47482 and HID 60441–60482 | no listeners |
| Remaining test processes | — | `ps` showed no QEMU, node, serial bridge, parec, or test ffmpeg |
| Original ISO | SHA-256 `f28171c3…490f2d` | unchanged |

No libvirt domain was started, stopped, defined, edited, or destroyed. No
Windows image or other VM image was opened for write. No `sudo`, network
download, package install, or commit was used.
