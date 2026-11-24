# Gate 2 live guest advanced report: ACPI, audio, D-Bus GL/Disable/secondary console

**Scope:** close the remaining guest-behaviour gaps on the disposable Alpine
Linux live ISO: ACPI/power semantics, guest-produced audio, and the QEMU D-Bus
display paths for GL, `Disable`, and a secondary console.

**Result:** ACPI, guest audio, and the D-Bus `Disable`/secondary-console paths
were exercised live. The important qualifications are:

| Area | Result |
|---|---|
| ACPI `system_powerdown` with no guest handler | **PASS (ignored)** — QEMU emits `POWERDOWN`, guest stays running |
| ACPI after starting Alpine's busybox `acpid` | **PASS** — guest prints `reboot: Power down` and QEMU exits |
| Node `POST /power/off` | **PASS with guest caveat** — ignored without `acpid`, powers off with it |
| Node `POST /power/stop` | **PASS** — QMP `quit`; QEMU process exits |
| Guest-initiated `reboot` through node `/input/type` | **PASS** — `reboot: Restarting system`; QEMU stays alive |
| Guest-initiated `poweroff` through node `/input/type` and manual QMP | **PASS** — `reboot: Power down`; QEMU exits |
| Guest audio | **PASS** — 440.1 Hz tone captured from QEMU's host audio monitor; peak −6.94 dBFS, tone RMS −9.96 dBFS |
| Node `/audio/*` routing | **PASS** — 523 Hz reference played to the null sink and captured; peak −26.89 dBFS |
| D-Bus 2D capture | **PASS** — BGRA `Scanout`/`Update`, snapshot and HLS |
| D-Bus `Disable` | **PASS on the GL path only** — QEMU emitted two `Disable` messages on reset; 2D VGA emitted none |
| D-Bus GL capture | **BLOCKED** — QEMU sends `ScanoutDMABUF`/`UpdateDMABUF`; the provider implements only BGRA `Scanout`/`Update` |
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
| Working tree | Dirty from concurrent sessions; this run added the D-Bus fix, this report, and short follow-up notes in existing reports |
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

### 5.2 Live GL capture — BLOCKED by DMABUF

The node selected `Console_0`:

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

It produced **no BGRA frame**, and no BGRA `Scanout` reached the source. The
provider implements only the BGRA `Scanout`/`Update`/`Disable` members, so it
could not produce a frame:

```text
hls: "failed"
hls_error: "HLS manifest ... has not been written yet"
GET /display/snapshot → 504 display.snapshot_timeout
QMP screendump → {"error":{"class":"GenericError","desc":"no surface"}}
```

The QMP screendump failure independently confirms that the GL display path has
no 2D surface to dump. **Exact blocker:** QEMU's `-display dbus,gl=on` uses
`ScanoutDMABUF`/`UpdateDMABUF`; `ozma-capture/src/dbus_display.rs` does not
implement the DMABUF members (it is a BGRA/pixman listener). This is the same
structural limitation recorded in the earlier D-Bus report, now confirmed live
with a guest and with named method logs.

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

The provider also now logs any unhandled method at debug level, which is how the
DMABUF names above were observed instead of silently disappearing.

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
$ cargo test -p ozma-capture --lib dbus_display
test result: ok. 26 passed; 0 failed; 1 ignored; 0 measured; 103 filtered out
```

No `ozma-power`/ACPI change was needed. `POST /power/stop` already calls QMP
`quit`; the live process-exit test confirms the documented force-off semantics.

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
| D-Bus GL capture | **FAIL / BLOCKED** | 11 `ScanoutDMABUF`, 1724 `UpdateDMABUF`, zero BGRA frames |
| D-Bus `Disable` on GL reset | **PASS** | two `Disable` messages, handler cleared framebuffer |
| D-Bus `Disable` on 2D reset/hotplug | **ABSENT, as expected** | no `Disable`; device_del not hotpluggable |
| `Console_1` selection | **PASS** | `virtio-gpu-pci.1` / `VGA` labels |
| `Console_1` live BGRA Scanout | **PASS with second VGA** | 720×400 JPEG + HLS; content early boot text |
| Guest-programmed second virtio-gpu head | **BLOCKED** | guest `Virtual-2` disconnected/disabled; no modesetting tool |
| Node `/input/type` shifted characters | **ANOMALY** | line stalled after `_`; unshifted `root`/`reboot`/`poweroff` worked |

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
3. **D-Bus GL is still not a capture surface.** The GL listener emits
   `ScanoutDMABUF`/`UpdateDMABUF`; the provider handles only BGRA
   `Scanout`/`Update`/`Disable`. `Disable` itself is live and handled, which
   closes the "QEMU never emits it" question for the GL path, but a GL guest
   cannot be captured without a DMABUF importer.
4. **Secondary console is live but not a guest second head.** `Console_1`
   selection and a live `Scanout` were demonstrated with a second VGA device.
   A `virtio-gpu-pci,max_outputs=2` guest kept `Virtual-2` disconnected and
   produced no second Scanout. The second VGA's captured content was stale
   early boot text because fbcon stayed on fb0.
5. **Secondary geometry signal bug fixed.** `Console_0`'s 1280×800
   `PropertiesChanged` was applied to `Console_1` (640×480). The path filter
   and its regression test are in `ozma-capture/src/dbus_display.rs`.
6. **Node shifted-character input anomaly.** A `/input/type` string containing
   `_` and `>` stopped appearing in the guest after the `_`; subsequent
   unshifted input also did not appear until the guest was restarted. Unshifted
   `root`, `reboot`, and `poweroff` worked repeatedly. This is recorded as an
   observed anomaly, not root-caused or fixed here.
7. **Not in this run:** WebRTC media, controller/dashboard, mouse-pointer
   artifacts (text console), and D-Bus GL with a DMABUF-capable importer.

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
