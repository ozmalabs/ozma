# Phase 3 live console-switch report

**Scope:** runtime `POST /display/select` on the Rust node with a real Alpine
3.21 live guest, two emulated display heads, QEMU D-Bus capture, HLS, and an
external WebRTC helper.

**Result: PASS for the D-Bus two-head switch, HLS/WebRTC restart, live frame
content change, HLS playlist continuity, rollback, invalid selection, and
single-head rejection.** One HLS continuity defect was found and fixed: a
console switch previously truncated the playlist and reset
`#EXT-X-MEDIA-SEQUENCE` to 0. It now appends with
`#EXT-X-DISCONTINUITY`, and that behavior is unit- and live-tested.

This is not a simultaneous-multi-console claim. The node still has one active
capture source and one live encoder pair at a time.

## 1. Environment

| Item | Value |
|---|---|
| Date | 2026-11-25 07:10–07:23 AEDT (2026-11-24 20:10–20:23 UTC) |
| HEAD | `72bad4ef48903aaef325c3f725a1a06f178626e6` (`72bad4e`) |
| QEMU | 11.1.3, KVM (`accel=kvm:tcg`) |
| ffmpeg | n9.1 |
| OCR | Tesseract 5.5.4 |
| Guest | Alpine Linux 3.21 live, kernel `6.12.13-0-virt`, root in tmpfs |
| ISO | `/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso`, SHA-256 `f28171c35bbf623aa3cbaec4b8b29297f13095b892c1a283b15970f7eb490f2d`, read-only |
| Temp root | `/tmp/opencode/phase3-console-switch-20261125-071052` (removed) |
| Node ports | HTTP 47831, QUIC/UDP 47833, HID/UDP 60481 |
| Build | clean detached worktree at `72bad4e` + the `ozma-node` patch below |

The main checkout acquired unrelated, uncommitted edits from concurrent
sessions during this run (`ozma-capture/src/dmabuf_egl.rs`, `ozma-usb/*`, docs),
and that tree did not compile. The live binary and the test gates were therefore
built from a detached clean worktree at `72bad4e` with only
`ozma-node/src/{display_control.rs,hls.rs}` copied in. No commit was made.

## 2. Guest and node commands

Two live heads were created with a virtio VGA plus a second emulated VGA:

```bash
qemu-system-x86_64 \
  -name ozma-phase3-console-switch \
  -machine pc,accel=kvm:tcg -cpu host -smp 2 -m 1024 -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" \
  -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modules=loop,squashfs,sd-mod,usb-storage,iso9660,virtio_gpu" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -device virtio-vga -device VGA \
  -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 \
  -display "dbus,p2p=yes" \
  -serial "file:$ROOT/qemu/serial.log" \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" -daemonize
```

The guest saw both framebuffers:

```text
$ cat /proc/fb
0 virtio_gpudrmfb
1 bochs-drmdrmfb
$ ls /sys/class/graphics
fb0 fb1 fbcon
$ cat /sys/class/graphics/fb1/name
bochs-drmdrmfb
```

Node command (D-Bus capture, both consoles advertised, HLS, WebRTC helper):

```bash
./target/debug/ozma-node \
  --name phase3-console-switch --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --capture-provider dbus --capture-console 0 \
  --dbus-width 1280 --dbus-height 800 --dbus-fps 10 \
  --input-provider qmp --power-provider qmp \
  --display-output "0:Console_0:1280x800:primary:dbus" \
  --display-output "1:Console_1:720x400:dbus" \
  --hls --hls-dir "$ROOT/run2/hls" --hls-fps 5 --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script /tmp/opencode/phase3_webrtc_helper.py \
  --webrtc-socket "$ROOT/run2/webrtc/signal.sock" \
  --webrtc-shm "$ROOT/run2/webrtc/frame.bgra" \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 5 \
  --http-port 47831 --hid-udp-port 60481 --cap video,hid
```

The WebRTC helper was a test-only Python worker that speaks the real
newline-delimited JSON signaling socket and records the PID, advertised
geometry, and MD5/size of each distinct BGRA shared-memory frame. The node's
real `WebRtcHelper` supervisor and frame feeder were exercised; this is not a
GStreamer media decode claim.

## 3. Both heads advertised

`GET /api/v1/connection` on startup:

```json
{
  "hls": "running",
  "hls_error": null,
  "webrtc": "running",
  "webrtc_error": null,
  "display_outputs": [
    {"index":0,"name":"Console_0","source_type":"dbus","capture_source_id":"dbus0","width":1280,"height":800,"primary":true},
    {"index":1,"name":"Console_1","source_type":"dbus","capture_source_id":"","width":720,"height":400,"primary":false}
  ],
  "display_capture": {"active_console":0,"provider":"dbus","limitation":"only Console_0 has a live capture source; ..."}
}
```

QEMU logged the selected console label and geometry (`label="virtio-vga"`,
1280x800), and the node received a BGRA `Scanout` for it. Only the active console
has a non-empty `capture_source_id`.

## 4. Live switch Console_0 -> Console_1

```http
POST /display/select {"index":1}
HTTP/1.1 200 OK
```

```json
{"ok":true,"active_console":1,"previous_console":0,"provider":"dbus",
 "changed":true,"restarted":{"hls":true,"webrtc":true},
 "display_capture":{"active_console":1,"provider":"dbus", ...}}
```

Node log:

```text
display console switched console=1 previous=0 provider="dbus" generation=1 restarted=["hls", "webrtc"]
```

Evidence:

| Signal | Console_0 before | Console_1 after |
|---|---|---|
| Snapshot | 1280x800, SHA-256 `9fc44a44…2821d` | 720x400, SHA-256 `820fd998…1aa27` |
| Snapshot OCR | `localhost login: root`, `CONSOLE0-PHASE3-MARKER`, `/proc/fb` output | Alpine kernel boot text (`Initialise system trusted keyrings`, `ttyS0 at I/O 0x3f8`, …) |
| HLS latest segment | 1280x800 | 720x400, SHA-256 `4b1b1dd6…bf22` |
| HLS frame OCR | shell + `CONSOLE0-PHASE3-MARKER` | same kernel boot text as the snapshot |
| WebRTC helper | PID `3767661`, 1280x800, 4,096,000 B/frame | PID `3768994`, 720x400, 1,152,000 B/frame, MD5 `1c6ad8a3…7503` |

The 720x400 snapshot and the decoded HLS segment are the second VGA's retained
early-boot scanout, not the guest login console. The guest kept fbcon on fb0
(`virtio_gpudrmfb`); writing `1` to `/sys/class/graphics/fbcon/map` did not move
tty1 output to fb1. This is the same honest second-VGA content limitation
recorded in `gate2-live-guest-advanced-report.md` §5.4B, but it is a real live
`Console_1` scanout and is visually distinct from the Console_0 login/shell.

## 5. Live switch back Console_1 -> Console_0

```http
POST /display/select {"index":0}
HTTP/1.1 200 OK
```

```json
{"ok":true,"active_console":0,"previous_console":1,"provider":"dbus",
 "changed":true,"restarted":{"hls":true,"webrtc":true}, ...}
```

| Signal | After switch back |
|---|---|
| Snapshot | 1280x800, SHA-256 `9fc44a44…2821d`, OCR again shows `CONSOLE0-PHASE3-MARKER`, `/proc/fb`, `END-DEVICES`, `FB1-PHASE3-MARKER`, and the shell prompt |
| HLS latest segment | 1280x800, SHA-256 `083fa265…0a9b` |
| HLS frame OCR | the same Console_0 shell markers |
| WebRTC helper | PID `3769516`, 1280x800, 4,096,000 B/frame |

## 6. HLS playlist continuity fix

The first live run (before the fix) showed that `HlsPipelineSwitch` stopped the
old pipeline and started a fresh one with first-generation ffmpeg flags. The
playlist was truncated and `#EXT-X-MEDIA-SEQUENCE` reset to 0:

```text
#EXT-X-MEDIA-SEQUENCE:0
seg_00000.ts … seg_00003.ts     # all 720x400, no discontinuity
```

That is a real client-visible continuity defect: the playlist URL keeps the same
name but its media sequence goes backwards, and players see an unmarked mode
change. The fix adds a restart-aware HLS generation path:

- `HlsPipeline::start_restarted_with_spawner_and_probe` passes `restart=true` to
  the first `spawn_generation` call, including the lazy first-frame spawn when a
  source reports `0x0`.
- `HlsController::start_restarted_with_source` is the console-switch entry point;
  `restart_or_start_with_source` also preserves append semantics when it
  replaces an existing pipeline.
- `HlsPipelineSwitch::restart_from` still stops the old console before opening
  the new one, then starts the replacement with append semantics.

After the fix, live playlists retained increasing media sequences and marked the
console boundary. During `Console_0 -> Console_1` (observed every 100 ms):

```text
07:21:00.324  #EXT-X-MEDIA-SEQUENCE:49
              seg_00049.ts
              seg_00050.ts
              seg_00051.ts
              #EXT-X-DISCONTINUITY
              seg_00052.ts        # first 720x400 segment
```

During `Console_1 -> Console_0`:

```text
07:21:16.052  #EXT-X-MEDIA-SEQUENCE:55
              seg_00055.ts
              seg_00056.ts
              seg_00057.ts
              #EXT-X-DISCONTINUITY
              seg_00058.ts        # first 1280x800 segment
```

Playlist snapshots were
`925286e1…77127` (first post-fix playable playlist at switch to Console_1) and
`e57d5cbd…d547f` (at switch back to Console_0). The final 1280x800 playlist was
`843f94ce…fbf577e` and its latest segment `seg_00068.ts` was
`b949080e…4a515`.

## 7. Forced pipeline failure and rollback

A second node run used a helper wrapper that deliberately exited 7 when the
helper was spawned for width 720. HLS still switched to Console_1 first; the
WebRTC restart then failed. `POST /display/select {"index":1}` returned:

```http
HTTP/1.1 502 Bad Gateway
```

```json
{"ok":false,"code":"capture.unavailable",
 "error":"restarting the webrtc pipeline for the requested console failed: WebRTC helper did not start ... (exit status: 7)",
 "details":{"pipeline":"webrtc","rolled_back":true},
 "remediation":"The previous console and pipeline were restored. ...",
 "retryable":true}
```

Post-rollback state:

```json
{"hls":"running","hls_error":null,"webrtc":"running","webrtc_error":null,
 "display_capture":{"active_console":0,"provider":"dbus"}}
```

The HLS playlist was back on 1280x800, the snapshot was 1280x800 with the
Console_0 marker, and the WebRTC helper was running again as PID `3776347` with
1280x800 / 4,096,000 B frames. `details.rolled_back:true` and an empty
`rollback_errors` list match the observed state. No success was claimed.

## 8. Invalid and single-head selections

On the D-Bus two-head node:

| Request | Result |
|---|---|
| `{"index":0}` while already active | `200`, `changed:false`, `restarted:{hls:false,webrtc:false}` |
| `{}` | `400 invalid_request`, `available:[0,1]`; state and pipelines unchanged |
| `{"index":2}` | `404 not_found`, `available:[0,1]`; state unchanged |
| `{"index":-1}` | `400 invalid-json` (`expected u32`); state unchanged |
| `{"id":"Console_9"}` | `404 not_found`, `requested:9`; state unchanged |

On a separate QMP single-head node (same QEMU guest, `--capture-provider qmp`,
two advertised outputs):

| Request | Result |
|---|---|
| `{"index":1}` | `409 conflict`, `details:{provider:"qmp",active_console:0,requested_console:1,single_head:true}` |
| `{"index":0}` | `200`, `changed:false`, `restarted:{hls:false,webrtc:false}` |
| `{"index":2}` | `404 not_found` |

## 9. Tests and checks

First in the clean worktree with the shipped `ozma-node` patch, then in the
main checkout after the unrelated concurrent edits settled:

```text
cargo fmt -p ozma-node -- --check
  clean worktree   PASS
  main checkout    PASS
cargo test -p ozma-node
  clean worktree   358 passed, 0 failed, 5 ignored
  main checkout    360 passed, 0 failed, 5 ignored
cargo check --workspace --all-targets
  clean worktree   PASS
  main checkout    PASS
```

The new regression test is
`hls::tests::controller_console_switch_generation_appends_the_existing_playlist`.
It asserts the startup generation uses plain `delete_segments`, while a
post-stop console-switch generation uses
`delete_segments+append_list+discont_start`.

During the live run the main checkout briefly did not compile because of
unrelated concurrent session edits to `ozma-capture/src/dmabuf_egl.rs` and
`ozma-usb/*`; the clean worktree was used for the live binary and the first gate
run. Those edits were fixed by their session and the final main-checkout gates
above passed.

## 10. Cleanup

- QMP and D-Bus nodes were stopped; the process needed `SIGKILL` after `SIGTERM`
  (see §11).
- The helper and ffmpeg children were freed with the node; no helper socket,
  shared-memory frame, QMP socket, or ffmpeg process was left behind.
- The QEMU guest was shut down and its PID file/socket removed.
- `/tmp/opencode/phase3-console-switch-*`, the clean worktree, the helper
  scripts, the worktree target, and the temporary evidence directory were
  removed.
- The read-only ISO and its hash were not modified. No sudo, download, package
  install, libvirt domain, or existing VM was used.

## 11. Remaining gaps / observations

- **One active console.** The node still has one capture registry source and one
  HLS/WebRTC pair; the other heads are advertised and selectable, not
  simultaneously encoded.
- **Second-VGA content is early boot text.** The guest fbcon remained on fb0
  (`virtio_gpudrmfb`). A guest-programmed second virtio-gpu scanout (or a
  reliable fbcon remap) is still unproven, so `Console_1` content is a distinct
  retained boot scanout rather than a second live login console.
- **WebRTC geometry is per-output metadata.** The switch updates the helper's
  width/height from the selected output spec. If the advertised geometry is
  wrong, the helper's frame-dimension check fails and rollback runs; it does not
  scale.
- **Node shutdown observation (outside this change):** after `SIGTERM` the node
  logged `Ozma node shutting down` but a worker remained at 99% CPU until
  `SIGKILL`. This occurred on both the pre-fix and post-fix D-Bus runs and is not
  attributed to console switching; it was not fixed here.
