# Gate 2 live HLS pacer and D-Bus depth report

**Scope:** live validation of the HLS wall-clock pacer (`ozma-node/src/hls.rs`)
and the QEMU D-Bus display capture depth (`ozma-capture/src/dbus_display.rs`).

**Environment:** direct, disposable `qemu-system-x86_64` processes only. No
libvirt domain was started, stopped, defined, edited, or destroyed, and no
`sudo` or package install was used. QEMU ran with TCG (`accel=tcg`), no disk
or network device, and was removed at the end.

> **Status:** HLS wall-clock pacing **PASS**; D-Bus `Scanout` **PASS**; live
> `Update` **PASS**; multi-console enumeration/selection **PASS** (secondary
> head has no `Scanout` without a guest); live `Disable` **BLOCKED** by QEMU's
> 2D display path and is unit-tested only. One live D-Bus bug was found and
> fixed: a VGA mode change sends `Update` bands wider than the last `Scanout`
> and previously killed the capture pipeline. The mid-session HLS
> resolution-change limitation found later (640×480 -> 1280×800) is fixed in
> code with unit tests; a live guest rerun is still required (§6.2).

| Item | Result |
|---|---|
| HLS pacer, QMP capture, `--hls-fps 5`, 1 s segments | **PASS** — 10 segments in 10.073 s (0.993/s), `#EXTINF:1.000000` |
| HLS pacer stall/no-burst | **PASS** — 2.5 s QEMU `SIGSTOP` paused the sequence; it resumed at ~1/s with no catch-up burst |
| D-Bus `Scanout` | **PASS** — 640×480 BGRA, 1,228,800 bytes |
| D-Bus `Update` | **PASS** — 658 live `Update` bands in the final run; HLS stayed running |
| D-Bus `Disable` | **BLOCKED** — not emitted by QEMU 11.1.3's 2D VGA listener; unit test only |
| D-Bus multi-console `Console_1` | **PASS** for selection; no `Scanout` without a guest |
| D-Bus + HLS across the initial mode switch | **PASS** after the fix — starts at 720×400, 1.000 s segments |
| Listener back-pressure (3 s consumer stall) | **PASS** bounded — QEMU stayed responsive and HLS recovered |

## 1. Environment and build

| Item | Value |
|---|---|
| Run | 2026-11-24 03:41–03:59 UTC (14:41–14:59 AEDT) |
| Git HEAD | `436b80c` |
| Working tree | clean when the pacer run began; other sessions changed unrelated files (`hls.rs` tests/docs/controller/softnode) during this run. Nothing was committed. |
| Build | `cargo build -p ozma-node` → exit 0 |
| QEMU | `qemu-system-x86_64` 11.1.3 |
| ffmpeg / ffprobe | n9.1 |
| Acceleration | TCG (`-machine pc,accel=tcg`); KVM was not used |
| Temp root | `/tmp/opencode/ozma-gate2-hls-dbus` (removed) |
| Ports | HTTP 47333–47340, HID/UDP 60271–60279 |

**Working-tree caveat.** The repository is shared with concurrent agent
sessions. At the start of this validation `git status --short` was empty.
During the run, other sessions added changes to `ozma-node/src/hls.rs`,
`docs/`, `controller/`, and `softnode/`. This report covers only
`ozma-capture/src/dbus_display.rs` and this new document. The pacer live run
used a binary built from the initially clean tree; later D-Bus rebuilds
included this report's `dbus_display.rs` fix and the concurrent unrelated
edits.

## 2. HLS wall-clock pacer — live PASS

### 2.1 QEMU and node

```bash
GDIR=/tmp/opencode/ozma-gate2-hls-dbus
qemu-system-x86_64 \
  -name ozma-gate2-hlsdbus \
  -machine pc,accel=tcg -m 256 -nodefaults \
  -device VGA -display none \
  -qmp "unix:$GDIR/qmp.sock,server=on,wait=off" \
  -pidfile "$GDIR/qemu.pid" -daemonize

./target/debug/ozma-node \
  --name ozma-gate2-hlsdbus --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --input-provider qmp --power-provider qmp \
  --http-port 47333 --hid-udp-port 60272 --cap hid,video \
  --hls --hls-dir "$GDIR/hls" \
  --hls-fps 5 --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

The node reported `hls: "running"` and exactly one shared QMP connection.
`ffprobe` on the rolling segments reported `duration=1.000000` for every
sampled segment. The node has no `--hls-segment-seconds` flag; its
`HlsConfig::default()` is 1.0 s, which is the value exercised here.

### 2.2 Playlist sampling

Twenty-one samples at 0.5 s intervals:

```text
span 10.073s  #EXT-X-MEDIA-SEQUENCE 6 -> 16  delta 10  rate 0.993/s
```

| Elapsed | Media sequence | `#EXTINF` | Head |
|---:|---:|---:|---|
| 0.00 s | 6 | 1.000000 | `seg_00009.ts` |
| 0.50 s | 7 | 1.000000 | `seg_00010.ts` |
| 1.01 s | 7 | 1.000000 | `seg_00010.ts` |
| 1.51 s | 8 | 1.000000 | `seg_00011.ts` |
| 2.01 s | 8 | 1.000000 | `seg_00011.ts` |
| 2.52 s | 9 | 1.000000 | `seg_00012.ts` |
| 3.02 s | 9 | 1.000000 | `seg_00012.ts` |
| 3.53 s | 10 | 1.000000 | `seg_00013.ts` |
| 4.03 s | 10 | 1.000000 | `seg_00013.ts` |
| 4.53 s | 11 | 1.000000 | `seg_00014.ts` |
| 5.04 s | 11 | 1.000000 | `seg_00014.ts` |
| 5.54 s | 12 | 1.000000 | `seg_00015.ts` |
| 6.04 s | 12 | 1.000000 | `seg_00015.ts` |
| 6.55 s | 13 | 1.000000 | `seg_00016.ts` |
| 7.05 s | 13 | 1.000000 | `seg_00016.ts` |
| 7.56 s | 14 | 1.000000 | `seg_00017.ts` |
| 8.06 s | 14 | 1.000000 | `seg_00017.ts` |
| 8.56 s | 15 | 1.000000 | `seg_00018.ts` |
| 9.07 s | 15 | 1.000000 | `seg_00018.ts` |
| 9.57 s | 16 | 1.000000 | `seg_00019.ts` |
| 10.07 s | 16 | 1.000000 | `seg_00019.ts` |

The sequence advances about once per wall-clock second, not about six times
per second. Every `#EXTINF` is 1.000000. There is no alternating burst of six
entries per second.

### 2.3 Stall/no-burst test

QEMU was frozen for 2.5 s with `SIGSTOP` while the node and playlist kept
running:

```text
SIGSTOP QEMU at 1795491721.170
SIGCONT QEMU at 1795491723.671

  0.00 s seq=46
  0.76 s seq=47
  1.77 s seq=48
  2.77 s seq=49
  6.05 s seq=50
  7.06 s seq=51
  8.06 s seq=52
  9.07 s seq=53
 10.08 s seq=54
```

The sequence pauses during the stall and resumes at one segment per second.
There is no catch-up burst. This is the expected `FramePacer` behavior: a late
frame is admitted once, then the next deadline is rebased to `now + interval`.

**Pacer verdict:** live wall-clock pacing matches the unit-tested contract at
`--hls-fps 5`. No `hls.rs` change was required.

## 3. D-Bus display depth

### 3.1 `Scanout` — live PASS

Fresh disposable QEMU with `-display dbus,p2p=yes`:

```bash
qemu-system-x86_64 \
  -name ozma-gate2-dbus-single \
  -machine pc,accel=tcg -m 256 -nodefaults \
  -device VGA -display dbus,p2p=yes \
  -qmp "unix:$GDIR/dbus-single/qmp.sock,server=on,wait=off" -daemonize

OZMA_DBUS_TEST_QMP="$GDIR/dbus-single/qmp.sock" \
  cargo test -p ozma-capture --lib -- --ignored --nocapture \
  real_qemu_dbus_display_pushes_a_frame
```

Result:

```text
live D-Bus Scanout: 640x480 1228800 bytes
test dbus_display::tests::real_qemu_dbus_display_pushes_a_frame ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 125 filtered out; ...
```

The node's shared-QMP D-Bus path also worked. With
`--capture-provider dbus --dbus-console 0 --hls`, the log showed:

```text
QMP control socket: one shared client for power, capture, snapshots, and input
D-Bus display connection authenticated id=dbus0
QEMU D-Bus console id=dbus0 label="VGA" width=720 height=400
D-Bus console properties received id=dbus0 width=720 height=400
D-Bus listener connection authenticated id=dbus0
D-Bus listener registered; awaiting Scanout id=dbus0
```

`ss -x` showed exactly one established pair on `qmp.sock`. HLS produced
segments and `hls: "running"`.

### 3.2 `Update` — live PASS (and a fixed mode-transition bug)

A fresh VGA node connects while SeaBIOS is still at 640×480 and then switches
to 720×400 text mode. QEMU 11.1.3 sends a console `PropertiesChanged`
(`Width=720`, `Height=400`) and then 720-wide `Update` bands, **without first
sending a fresh 720×400 `Scanout`**. Before the fix, the source treated the
first `Update` as a fatal geometry error:

```text
capture source `dbus0` failed: invalid display geometry:
Update rectangle 0,0 720x16 exceeds the 640x480 framebuffer
```

The final node command for this run was:

```bash
./target/debug/ozma-node \
  --name ozma-gate2-dbusearly2 --profile vm \
  --qmp-socket "$GDIR/dbus-early2/qmp.sock" \
  --capture-provider dbus --dbus-console 0 --dbus-fps 5 \
  --input-provider qmp --power-provider qmp \
  --http-port 47337 --hid-udp-port 60276 --cap hid,video \
  --hls --hls-dir "$GDIR/dbus-early2/hls" \
  --hls-fps 5 --hls-bitrate-kbps 1000
```

After the fix, the final live run started the capture before the mode switch
and logged:

```text
D-Bus console properties received id=dbus0 width=640 height=480
D-Bus listener registered; awaiting Scanout id=dbus0
D-Bus console size signal received id=dbus0 width=720 height=400
D-Bus console geometry changed; resizing Scanout id=dbus0 width=720 height=400
```

The resulting HLS stream was 720×400:

```text
$ cat .../hls/stream.m3u8
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:9
#EXTINF:1.000000,
seg_00009.ts
...

$ ffprobe -v error -select_streams v:0 \
    -show_entries stream=width,height -show_entries format=duration \
    -of default=nw=1 .../hls/seg_00011.ts
width=720
height=400
duration=1.000000
```

Evidence from the QEMU D-Bus wire trace (`strace` on QEMU):

```text
      1 PropertiesChanged
      1 Scanout
    658 Update
```

The 658 `Update` messages were received and applied to the source
framebuffer; HLS stayed `running` (the 5 fps pacer intentionally drops surplus
frames before ffmpeg).

This is a real live `Update` validation, not an inference from the unit test.
The previously reported "Scanout only" gap is closed for `Update`.

### 3.3 `Disable` — live BLOCKED, unit-tested only

Attempts and results:

| Attempt | Result |
|---|---|
| `system_reset` (QMP) | Re-delivered `Update` bands; no `Disable` |
| `system_powerdown` (QMP) | `POWERDOWN` event; no guest to power off; no `Disable` |
| `device_del {"id":"vga0"}` | `Device 'VGA' does not support hotplugging` |
| `device_del {"id":"gpu0"}` (virtio-gpu-pci) | `Device 'virtio-gpu-pci' does not support hotplugging` |

The blocker is structural, not a missing test command. In QEMU 11.1.3
`ui/dbus-listener.c`:

* `Disable` is sent only by `dbus_scanout_disable()`, which is compiled under
  `CONFIG_OPENGL`.
* `dbus_scanout_disable` is registered only in `dbus_gl_dcl_ops`
  (`dpy_gl_scanout_disable`). The non-OpenGL `dbus_dcl_ops` used by a 2D
  `-device VGA` has no such callback.
* Therefore a 2D VGA capture never emits `Disable`, regardless of a guest.
* Forcing the GL path (`-display dbus,gl=on`) makes QEMU use
  `dbus_gl_dcl_ops`, which sends `ScanoutDMABUF`/`UpdateDMABUF`/`Disable`;
  this BGRA/pixman listener implements only `Scanout`/`Update`/`Disable`, not
  the DMABUF members, so a guest on that path is not a supported capture
  surface here.

The `Disable` handler (`framebuffer = None`, zero format, no frame emitted) is
covered by `disable_drops_the_framebuffer_without_emitting_a_frame`; this is
a unit-test claim, not a live claim.

### 3.4 Multi-console enumeration and selection

QEMU with two virtio-gpu heads:

```bash
qemu-system-x86_64 \
  -name ozma-gate2-dbus-multi \
  -machine pc,accel=tcg -m 256 -nodefaults \
  -device virtio-gpu,max_outputs=2 \
  -display dbus,p2p=yes \
  -qmp "unix:$GDIR/dbus-multi/qmp.sock,server=on,wait=off" -daemonize
```

| `--dbus-console` | Reported console | Listener |
|---|---|---|
| `0` | `label="virtio-gpu-pci.0"`, 640×480 | authenticated and registered |
| `1` | `label="virtio-gpu-pci.1"`, 640×480 | authenticated and registered |

Selection works: `Console_1` resolves to the second virtio-gpu output, not to
the default. With neither head driven by a guest, QEMU sent no `Scanout`, so
HLS correctly reported:

```text
hls_error: "HLS manifest `.../hls/stream.m3u8` has not been written yet"
```

That is a no-guest limitation of the test, not a console-selection bug. A live
secondary-head `Scanout` still requires a guest that programs that output.

### 3.5 Listener back-pressure (bounded)

With the D-Bus + HLS node running, the node process was frozen with
`SIGSTOP` for 3.045 s, then resumed. QMP on a separate control socket was
queried during the stall:

```text
qmp during (1s): {'return': {'status': 'running', 'running': True}} 1ms
qmp during (3s): {'return': {'status': 'running', 'running': True}} 1ms
hls before stall: media sequence 34
hls after resume:  media sequence 38 (four wall seconds later)
hls after resume:  "running", no error
```

QEMU's GDBus send path did not deadlock while the listener was unread; the
node resumed, drained the queued messages, and the pacer did not burst. This
is a bounded 3 s test only. Longer stalls, kernel socket-buffer limits, and
QEMU outgoing-queue memory growth were not measured.

## 4. Fix applied

Files changed: `ozma-capture/src/dbus_display.rs` (plus this report). No
`ozma-node/src/hls.rs` change was needed for pacing.

1. **Console geometry tracking.** The background console drain now parses
   `org.freedesktop.DBus.Properties.PropertiesChanged` for
   `org.qemu.Display1.Console` and records the latest `Width`/`Height`.
2. **Mode-transition resize.** A pending console size is applied to a new
   `Scanout` and to the next `Update`. `Framebuffer::resize()` preserves the
   overlapping top-left pixels and zero-fills newly exposed area.
3. **A non-fitting `Update` is no longer fatal.** If no console-size signal has
   arrived yet, the source grows the framebuffer to the damaged band's extent
   and applies the update. If that still fails, it drops the stale surface and
   waits for a new `Scanout` instead of ending the capture pipeline.
4. **`Update` before `Scanout` waits** for the full frame instead of returning
   `QEMU sent Update before any Scanout`.
5. **`Disable` is logged** when received; behavior is unchanged (clear the
   framebuffer, emit no frame).

New/updated unit tests:

* `update_before_scanout_waits_for_a_scanout`
* `oversized_update_grows_the_framebuffer_instead_of_failing`
* `console_properties_changed_signal_carries_width_and_height`
* `framebuffer_resize_preserves_the_overlap_and_zero_fills`

## 5. Tests and checks

```text
$ cargo test -p ozma-capture --lib dbus_display
test result: ok. 25 passed; 0 failed; 1 ignored; ... finished in 0.00s

$ cargo test -p ozma-capture --lib
test result: ok. 125 passed; 0 failed; 4 ignored; ... finished in 2.72s

$ cargo test -p ozma-node --lib hls::
test result: ok. 30 passed; 0 failed; 1 ignored; ... finished in 0.01s

$ cargo check -p ozma-capture -p ozma-node
Finished `dev` profile ... in 1.72s

$ rustfmt --edition 2021 ozma-capture/src/dbus_display.rs
(applied; no diff after formatting)
```

`cargo clippy` is not installed in this toolchain, so clippy was not run.

## 6. Remaining depth limitations

* **`Disable` cannot be exercised on the 2D VGA path.** QEMU 11.1.3 only
  calls `dpy_gl_scanout_disable` for the OpenGL listener, and this source
  implements the non-GL BGRA/pixman listener. A live `Disable` test would
  require implementing QEMU's DMABUF listener members and a GL-capable guest.
* **Secondary head needs a guest.** Enumeration and selection of `Console_1`
  work, but no `Scanout` is produced until a guest programs that output.
* **Mid-session resolution changes are now handled by HLS, pending a live
  rerun.** The D-Bus source emits correctly resized frames. Previously
  `hls.rs::run_pipeline` fixed its input dimensions from the first frame and
  rejected a later size change; in the final pre-fix run the console
  `PropertiesChanged` arrived before the first `Scanout` was handed to HLS, so
  the first frame was already 720×400. `run_pipeline` now closes and reaps the
  old ffmpeg child and starts a new generation with the new `-s WxH` when
  `HlsConfig::width`/`height` are unset, appending the playlist with
  `-hls_flags delete_segments+append_list+discont_start` and rebasing
  `FramePacer`. Pinned dimensions remain a typed `HlsError::FrameDimensions`
  failure. This is fake-spawner unit-test evidence; the live 640×480 ->
  1280×800 D-Bus transition has **not** been re-run since the fix.
* **Formats:** only 32-bit BGRA/pixman formats are accepted. DMABUF/GL and
  non-32-bpp formats are rejected rather than mislabelled.
* **Input:** D-Bus input is not implemented; the node uses uinput/QMP/HID.
* **Ignored real test:** `real_qemu_dbus_display_pushes_a_frame` still needs
  an exclusive QMP socket (run it before, or instead of, a node that owns the
  shared QMP connection).
* **Back-pressure:** only a 3 s bounded stall was tested; no long-stall memory
  or queue-growth measurement was made.

### 6.1 Documentation follow-up

This report supersedes the "live rerun pending" language for HLS pacing in
[`gate2-live-qmp-report.md`](gate2-live-qmp-report.md) §0.9 and the
"`Update`/`Disable` are unit-tested, not live" sentence in the D-Bus row of
[`parity-matrix.md`](parity-matrix.md): `Update` is now live-validated, while
`Disable` is structurally blocked on the 2D path. `docs/gate2-validation.md`
also still carries the older claims that HLS timestamps use the configured fps
rather than wall-clock time and that the argv has no `delete_segments` flag.
Those documents were being edited by other concurrent sessions during this
run, so they are left for the parent session to reconcile rather than edited
here.

### 6.2 HLS resolution-change fix (code landed, live rerun pending)

`ozma-node/src/hls.rs` now keeps the pipeline alive across a frame-size change
when `HlsConfig::width`/`height` are unset (the live node's `--hls` path passes
neither):

* the old ffmpeg child's stdin is closed, the child is killed and awaited, and
  a new generation starts with the new `-s WxH` and the same encoder, bitrate,
  segment duration, playlist size, and output directory;
* restart argv carries
  `-hls_flags delete_segments+append_list+discont_start`, so the existing
  playlist is appended and the switch is marked `#EXT-X-DISCONTINUITY`; ffmpeg
  continues segment numbering from the last entry (probed with n9.1, including
  a mid-run `SIGKILL`);
* `FramePacer::rebase` schedules the next frame one interval after the
  completed restart, so media time cannot burst;
* a failed restart returns a typed `HlsError::Restart`, marks status `failed`,
  and stops feeding the dead child;
* pinned `width`/`height` reject a different frame size with a typed
  `HlsError::FrameDimensions` instead of silently scaling.

Unit tests (fake source/spawner, no real ffmpeg): 640×480 then 1280×800 starts
a second ffmpeg with `-s 1280x800`, closes the old sink, and adds
`append_list+discont_start`; pinned dimensions fail with `FrameDimensions`; a
failed second spawn reports `HLS restart failed` and leaves only one write;
`FramePacer::rebase` discards catch-up debt. The command
`cargo test -p ozma-node --lib hls::` reports 34 passed, 1 ignored. The live
640×480 -> 1280×800 D-Bus run
recorded in §3.2 was performed **before** this change and must be repeated
against a guest to claim the transition live.

## 7. Cleanup

All processes created by this validation were terminated and the temp root was
removed:

```text
TERM node 3977083  (dbus-early2)
TERM node 3979834  (dbus-devdel)
TERM node 3980505  (dbus-devdel-gpu)
TERM qemu 3977079 / strace 3977075
TERM qemu 3979820 / strace 3979816
TERM qemu 3980491 / strace 3980487

$ test -e /tmp/opencode/ozma-gate2-hls-dbus && echo EXISTS || echo ABSENT
ABSENT

$ ss -lntup | grep -E ':(47333|47334|47336|47337|47339|47340|60271|60272|60273|60275|60276|60278|60279)\b'
(no rows)

$ ss -lx | grep -F 'ozma-gate2-hls-dbus'
(no rows)

$ ps -C qemu-system-x86_64 -o pid,args | grep -- '-name ozma-gate2-dbus'
(no rows)

$ ps -C ozma-node -o pid,args | grep 'ozma-gate2-dbus'
(no rows)

$ ps -C ffmpeg -o pid,args | grep 'ozma-gate2-hls-dbus'
(no rows)
```

Processes belonging to other concurrent sessions (for example
`ozma-gate2-guest`, `ozma-gate2-guest-dbus`, `ozma-gate2-ctrl-webrtc`, and
`ozma-gate2-hw`) were left untouched. No libvirt domain and no existing VM was
used. Nothing was committed.
