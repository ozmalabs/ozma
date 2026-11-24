# Gate 2 live device report — audio router and uinput/evdev

Live validation of the **real command and device paths** behind Gate 2 audio
sink provisioning and per-VM evdev provisioning, run against the host described
in [`gate2-environment-report.md`](gate2-environment-report.md).

**Scope:** disposable null sinks and disposable uinput devices only. No sudo, no
package installs, no persistent configuration, no hardware-sink or hardware-input
changes, no real desktop input grabs, no input events injected. Gate 2 itself
(VM + QEMU + controller end-to-end) is **not** claimed here.

| Item | Value |
|---|---|
| Date | 2026-11-24, approximately 13:15–13:30 AEDT |
| Host | `SFFPC`, CachyOS, kernel `7.3.5-2-cachyos` x86_64 |
| Repo | `/home/matt/ozma/ozma`, `HEAD=38e7dfa`, working tree clean before and after |
| User | `uid=1000(matt)`, groups include `audio`, `video`, `libvirt`; **not** `input` |
| Audio server | `pactl info` → PulseAudio (on PipeWire 1.6.11), server protocol 35 |
| Tools | `pactl` 17.0-134-g77d25, `pw-play` libpipewire 1.6.11, `paplay`/`pacat` 17.0-134-g77d25 |
| uinput | `/dev/uinput` present, `crw-rw----+ root:input 10,223`, `uinput` module loaded |

All temporary probe sources and compiled probe binaries were deleted after the
run. The working tree is clean apart from this report.

## 1. Audio path — live result: PASS

### 1.1 Host checks

```console
$ pactl info
Server String: /run/user/1000/pulse/native
Server Name: PulseAudio (on PipeWire 1.6.11)
Server Version: 15.0.0
Default Sink: alsa_output.usb-Roland_Rubix22-00.analog-stereo.99

$ pactl list short sinks
20612  alsa_output.pci-0000_13_00.6.analog-stereo.99      PipeWire  s32le 2ch 48000Hz  SUSPENDED
20614  alsa_output.usb-Roland_Rubix22-00.analog-stereo.99 PipeWire  s32le 2ch 48000Hz  RUNNING

$ pw-play --version
pw-play
Compiled with libpipewire 1.6.11
Linked with libpipewire 1.6.11

$ paplay --version
pacat 17.0-134-g77d25
Compiled with libpulse 17.0.0
Linked with libpulse 17.0.0

$ pactl --version
pactl 17.0-134-g77d25
```

### 1.2 Router entry point used

A temporary `ozma-audio` example (now deleted) constructed the production
router types directly:

- `PipeWireAudioRouter::with_config(Arc<TokioCommandRunner>, RouterConfig::with_track_root(..))`
- `PulseAudioAudioRouter::with_config(Arc<TokioCommandRunner>, RouterConfig::with_track_root(..))`

It called only the real `AudioRouter` methods; no fake runner and no mocked
commands. A disposable sink named `ozma-gate2-test-3714220` was used. The
reference track was a generated 3.0 s, 440 Hz, stereo 48 kHz WAV
(`ozma-gate2-tone.wav`, 576,078 bytes) produced with `ffmpeg`.

### 1.3 Observed results

| Step | Result | Evidence |
|---|---|---|
| `ensure_sink` | **PASS** | `created: true`; sink appears in `pactl list short sinks` |
| Monitor target | **PASS** | `ozma-gate2-test-3714220.monitor` appears in `pactl list short sources` |
| Module discoverability | **PASS** | `pactl list short modules` shows `module-null-sink` with `sink_name=ozma-gate2-test-3714220` |
| Apply profile | **PASS** | sink index `26998`; volume `37%`, `Mute: yes` |
| Remove profile | **PASS** | volume restored to `100%`, `Mute: no` |
| `pw-play` playback | **PASS** | new sink-input `27017` on sink index `26998` |
| `stop_playback` (`pw-play`) | **PASS** | sink-input gone; no `pw-play` process remains |
| `paplay` playback | **PASS** | new sink-input on sink index `26998` |
| `stop_playback` (`paplay`) | **PASS** | sink-input gone; no `paplay` process remains |
| `remove_sink` | **PASS** | sink, monitor source, and `module-null-sink` module all absent |

Captured while `pw-play` was active:

```text
21761  20614  21760  PipeWire  float32le 2ch 48000Hz   <- pre-existing, untouched
27017  26998  27016  PipeWire  s16le 2ch 48000Hz       <- disposable sink
```

`21761` was already on the hardware sink before the probe and was never moved.
`move_stream` was deliberately **not** called, per the “move nothing” constraint,
so the live move-sink-input path remains untested (unit-tested only).

The probe’s own final line was `RESULT PASS (0 failure(s))`, exit code `0`.

### 1.4 Audio cleanup

- `remove_sink` unloaded the owning module by discovering its id from
  `pactl list short modules` (not from a cached creation id).
- Post-run `pactl list short sinks` contains only the two original hardware
  sinks (`20612`, `20614`).
- No `module-null-sink` remains.
- No `pw-play` or `paplay` process remains.
- `pactl list short sink-inputs` contains only the pre-existing stream `21761`.
- `ozma-gate2-test-3714220.monitor` no longer exists.

## 2. Evdev/uinput path — live result: PASS (no injection)

### 2.1 Permissions

```console
$ ls -l /dev/uinput
crw-rw----+ 1 root input 10, 223 /dev/uinput

$ getfacl /dev/uinput
user::rw-
user:matt:rw-          # access is granted by an ACL, not by group membership
group::rw-
other::---

$ id
uid=1000(matt) ... groups=... 996(audio), 985(video), 956(libvirt), ... (no `input`)

$ getent group input
input:x:994:brltty

$ lsmod | grep '^uinput'
uinput  28672  3
```

Strictly, `test -w /dev/uinput` succeeds because of the per-user ACL. `matt` is
**not** in the `input` group. On a host without that ACL, the documented
requirement is group `input` membership or a udev `uaccess`/TAG rule; without it
the open fails with `EACCES` and no `/dev/input/eventN` is produced.

### 2.2 `UinputEvdevProvisioner` — real create/verify/release

A temporary `ozma-softnode` example (now deleted) included the real
`src/evdev_provisioning.rs` and used the production
`UinputEvdevProvisioner` with `enabled: true`. To avoid writing host paths, the
marker and symlink directories were redirected to
`/tmp/opencode/gate2-evdev/{run,by-id}`. The VM name was
`gate2probe-3722808`.

| Check | Result | Evidence |
|---|---|---|
| Create keyboard + pointer | **PASS** | `ensure` returned `/dev/input/event22` and `/dev/input/event23` |
| Real event nodes | **PASS** | both are character devices with `major 13` |
| Device identity | **PASS** | sysfs names `ozma-kbd-gate2probe-3722808`, `ozma-mouse-gate2probe-3722808` |
| Marker files | **PASS** | `<marker_dir>/gate2probe-3722808.{kbd,mouse}` contain the exact event paths |
| Stable symlinks | **PASS** | `by-id/ozma-kbd-…` and `by-id/ozma-mouse-…` resolve to the event nodes |
| `release` | **PASS** | event nodes, markers, and symlinks all removed |
| Residue | **PASS** | no sysfs name or event node remains |

The probe never called `emit`, so no keystroke, click, or pointer motion was
sent to any desktop session.

### 2.3 `UinputSink` — open/drop without injection

A second temporary example (now deleted) used the real `ozma_input::UinputSink`
with unique names:

- keyboard `ozma-gate2-uinput-kbd-3723206`
- pointer `ozma-gate2-uinput-mouse-3723206`
- protocol `Absolute` (the default)

Both `/dev/input/event22` and `/dev/input/event23` appeared immediately, were
visible in sysfs under those names, and disappeared when the sink was dropped.
No `submit` or `release_all` call was made.

### 2.4 Ignored real-device test — SKIPPED for safety

`ozma-input/src/uinput.rs::real_uinput_device_accepts_events`
(`#[ignore]`, needs `OZMA_UINPUT_TEST=1`) was **not executed**. It calls
`submit` with:

- key `0x04` down,
- left mouse button down,
- wheel event,
- absolute pointer move to `(100,100)`.

On this host an active seat0/tty1 session and real “Mouse passthrough” /
“Keyboard passthrough” devices are present. A newly created uinput node can be
picked up by the compositor’s libinput, so that sequence could move or click the
real desktop pointer. The two probes above validate the same open/create/drop
path with **zero** events injected. Event delivery itself remains unverified on
purpose.

### 2.5 Production-path gap: default directories

The live probe used `/tmp` because the production defaults are not writable by
`uid 1000`:

```console
$ ls -ld /run /dev/input/by-id
drwxr-xr-x 39 root root 900 /run
drwxr-xr-x  2 root root 200 /dev/input/by-id
$ test -w /run || echo "/run NOT writable"
/run NOT writable
```

`/run/ozma` does not exist and cannot be created by this user. In
`UinputEvdevProvisioner::create`, a marker-write failure is **fatal**
(`EvdevProvisionError::Marker`), so the default
`marker_dir = /run/ozma/evdev` would fail here unless the supervisor runs with
privilege or the directory is pre-created and owned appropriately. The
`/dev/input/by-id` symlink is best-effort (failure is logged, not fatal).

## 3. Pass / fail / skip summary

| Check | Status |
|---|---|
| `pactl info` + live Pulse-on-PipeWire server | **PASS** |
| `pw-play` / `paplay` / `pactl` present and runnable | **PASS** |
| `PipeWireAudioRouter` create sink + monitor target | **PASS** |
| Volume/mute apply and restore on a disposable sink | **PASS** |
| `pw-play` reference playback and stop | **PASS** |
| `paplay` reference playback and stop | **PASS** |
| Sink/module removal | **PASS** |
| `/dev/uinput` write access | **PASS** (host-specific ACL) |
| `UinputEvdevProvisioner` create/verify/release | **PASS** |
| `UinputSink` open/verify/drop | **PASS** |
| Ignored real-device event test | **SKIP** (would inject into desktop) |
| Live `move-sink-input` | **SKIP** (instructed to move nothing) |
| Audible/non-silent output | **SKIP** (null sink only; no monitor capture) |
| Guest input delivery | **SKIP** (no VM/QEMU/guest) |
| Microphone/source path | **SKIP** (router rejects source routing; no capture opened) |
| Native PipeWire node creation | **SKIP** (implementation uses `pactl` by design) |

## 4. Cleanup confirmation

Verified after the probes:

- `pactl list short sinks` → only the original `20612` and `20614`.
- `pactl list short modules | grep null-sink` → no output.
- `pactl list short sink-inputs` → only the pre-existing `21761`; the disposable
  stream is gone.
- `pgrep -x pw-play` / `pgrep -x paplay` → no processes.
- No `/dev/input/event22` or `/dev/input/event23`; no `gate2probe`,
  `ozma-kbd-gate2`, `ozma-mouse-gate2`, or `ozma-gate2-uinput` name in sysfs.
- `/tmp/opencode/gate2-evdev` and `/tmp/opencode/gate2-audio` removed.
- Temporary example sources removed from `ozma-{audio,softnode,input}/examples/`;
  their compiled binaries under `target/debug/examples/` removed.
- `git status --porcelain` is clean (this report is the only authored file).

One host artifact could not be removed without privilege: the first wrapper
script killed its own `bash` via a too-broad `pkill -f` pattern, producing
`/var/lib/systemd/coredump/core.bash.1000.b5099eabd2224bebb43d59ddb2532af6.3715355.1795486676000000.zst`
(2.2 MiB, root-owned, mode `640`). It is not a sink, module, device, temp file,
or process from the validation, and `sudo` was prohibited. The probe itself had
already exited `0` (`RESULT PASS`); the rerun with a safe trap exited `0`.

## 5. Remaining gaps and next validation step

Not proven by this run:

1. **Audible output / non-silent samples.** The null sink is intentionally
   silent. Sink-input presence proves routing, not audio data. Next: capture
   `<sink>.monitor` with `pw-record`/`parec` while playing a known tone and
   assert a non-zero RMS/peak; then repeat on the real hardware sink with
   operator confirmation of audibility.
2. **Guest input.** No domain is running and no `input-linux` object was
   attached. Next: start a disposable domain, attach the provisioned
   `/dev/input/eventN` paths, send one non-destructive key/pointer event to the
   **guest only**, and verify via a guest-side agent or a framebuffer/screenshot
   delta. Do not inject into the host seat.
3. **Mic path.** `AudioProfile::source` is rejected with `AudioError::Unsupported`
   by design, and no source/monitor capture was opened. Next: open
   `PipeWireCapture::open_sink_monitor` (or a real mic source) and verify frames
   with the existing Opus path.
4. **Native PipeWire node creation.** The router intentionally uses
   `pactl load-module module-null-sink` through `pipewire-pulse`; it does not use
   `pw-cli create-node adapter`. Native node creation remains unimplemented and
   untested.
5. **Default provisioning directories.** As found in §2.5, `/run/ozma/evdev`
   and `/dev/input/by-id` need privilege or pre-created ownership for the
   production defaults to work under `uid 1000`. Next: run the supervisor with
   the intended service user and a udev rule, and verify marker creation plus
   the by-id symlink.
6. **Live stream move.** `pactl move-sink-input` was not exercised live because
   the task required moving nothing. Next: spawn a disposable `pw-play` stream,
   move only that stream onto a disposable null sink, verify the sink-input
   target changed, then move it back or stop it.

The next concrete Gate 2 step is therefore: **bring up one disposable VM, attach
the provisioned evdev nodes and the disposable null sink to it, and verify guest
input plus monitor-captured non-silent audio** — with the same cleanup discipline
used here.
