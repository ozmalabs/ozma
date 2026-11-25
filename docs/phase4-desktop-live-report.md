# Phase 4 desktop profile live runtime report

Date: 2026-11-26 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
HEAD: `f6d0260feb3b458327d0b1f3da4611c2c5be5943`
Binary: existing `target/debug/ozma-node` (`ozma-node 0.1.0`, mtime
2026-11-25 23:42 local)

**Overall result: PARTIAL PASS / FAIL on two desktop controls.** Real X11
capture, `/video`, HLS, uinput input, display geometry, wallpaper, PipeWire
routing/playback, and graceful SIGTERM all worked. `/clipboard` writes return a
typed 502 after a 5 s timeout even though the text lands, and the command leaks
an orphan `xclip`. `--x11-display` is not applied to the clipboard/wallpaper
child processes, so those routes silently depend on ambient `DISPLAY` and
misreport availability when it is unset. No product code was changed and no
commit was created.

This report is the live evidence for the desktop profile; it does not re-evaluate
the parity matrix or the Phase 4 completion gates.

## Follow-up (2026-11-26)

The live failures below were fixed after this run; the body of this report
records the original evidence and is not rewritten. See
[`phase4-node-live-fixes-report.md`](phase4-node-live-fixes-report.md) for the
implementation and new live evidence.

* **Clipboard write/owner leak: fixed.** Completion now depends on the direct
  child's exit, not inherited-pipe EOF; clipboard writes run in an isolated
  process group with a unique marker, and the previous owner is terminated on a
  new write and on node shutdown. A second live run with ambient `DISPLAY` unset
  and `--x11-display :97` returned 200 for both writes, kept exactly one
  `xclip` owner, replaced its pid on the second write, and left no owner after
  SIGTERM.
* **`--x11-display` propagation: fixed.** `xclip`/`xsel`, `feh`, and
  `xwallpaper` now receive the resolved `DISPLAY` (and `swaybg` receives
  `WAYLAND_DISPLAY`). `feh` is run with `--no-fehbg`. An unreachable explicit
  display returns typed `503 desktop.no_display` for clipboard and wallpaper.
* **`GET /wallpaper` shape: fixed.** It now reports the resolved `display` and
  `last_wallpaper` in addition to `x11_display`/`last_applied`, with
  `verified:false`.
* The original `502` and false-availability observations remain below as
  historical evidence from the pre-fix binary.

## Safety and environment

- A private Xorg 1.21.1.26 server on `:90` used the `dummy` driver at
  800x600x24. `:0` and the real seat Xorg (`vt2`, `/run/sddm/xauth_*`) were never
  addressed.
- The final throwaway Xorg ran in a private mount namespace (`unshare -m
  --propagation private`) with an empty directory bind-mounted over
  `/usr/share/X11/xorg.conf.d`. Only `MatchProduct "ozma-keyboard"` and
  `MatchProduct "ozma-mouse"` InputClasses were loaded. `xinput list` before the
  node showed only the XTEST devices; after the node it showed only
  `ozma-keyboard`/`ozma-mouse`. No physical input was admitted by the final
  server.
- Transparency note: an initial unnamespaced Xorg probe (before the restricted
  config) auto-added the host's physical libinput devices read-only; it was
  stopped within seconds, no event was injected, and it was replaced by the
  namespaced server above. No GPU output, real display, or default audio device
  was used at any point.
- `/dev/uinput` already granted the invoking user access; no udev rule or group
  change was made:
  `getfacl /dev/uinput` -> `user:matt:rw-`, group `ozma-node`.
- Test packages installed for the run (authorized by the task): `xclip 0.13`,
  `feh 3.13.1`, `xterm 411`. They were left installed; no persistent service,
  udev, or permission change was made.
- A first attempt used Xvfb `:90`. Xvfb does not hotplug a new evdev/uinput
  device, so node `/input/*` calls reached the kernel but never the X client.
  Xvfb was replaced with the namespaced Xorg dummy server above. This is a
  harness limitation, not a node bug.
- A harness-side timeout killed Xvfb once; the HLS pipeline then reported
  `failed` and appended `#EXT-X-ENDLIST`. The final run used detached
  (`setsid`) processes and is the evidence below.

## Node invocation

```bash
DISPLAY=:90 ./target/debug/ozma-node \
  --profile desktop --name ozma-desktop-live \
  --http-port 17482 --hid-udp-port 17483 \
  --capture-provider x11 --x11-display :90 \
  --x11-width 800 --x11-height 600 --x11-fps 15 \
  --input-provider uinput \
  --audio-provider pipewire --audio-router pipewire \
  --audio-capture-target monitor:ozma_codex_test \
  --audio-track-root /tmp/opencode/ozma-desktop-live/audio \
  --audio-sink ozma_codex_test \
  --hls --hls-dir /tmp/opencode/ozma-desktop-live/hls \
  --hls-fps 15 --hls-bitrate-kbps 1200 \
  --api-token ozma-live-token
```

Startup selected `capture=x11`, `input=["uinput"]`, `audio=pipewire`, and
`hls=running`. `/api/v1/connection` reported:

```json
{"capture":{"provider":"x11","kind":"x11","source":"x11","available":true},
 "input":{"sink_kind":"uinput","available":true,"injecting":true},
 "audio":"running","hls":"running"}
```

`desktop_controls` advertised `x11_display: ":90"` with `geometry`,
`clipboard`, and `wallpaper` all available (`xrandr`, `xclip`, `feh`).

## Results

| Check | Result | Evidence |
|---|---|---|
| Real X11 capture | **PASS** | `/display/info` -> `source_id=x11`, `800x600`, `active=true`, `snapshot_available=true`, `last_error=null`. |
| Snapshot shows test window | **PASS** | `/display/snapshot` -> 800x600 JPEG, 13,822 bytes; pixel `(100,100)` inside the magenta xterm was `srgb(255,0,254)`; 1,375 colours. |
| `/video` MJPEG | **PASS** | `GET /video?fps=5&quality=70` streamed 125,810 bytes in 2 s; first JPEG 800x600, pixel `(100,100)` `srgb(255,0,255)`. |
| HLS segments decode | **PASS** | Playlist media sequence advanced; `ffprobe` on the newest segment: `h264`, 800x600, 15 fps, `nb_read_frames=15`; `ffmpeg -i seg.ts -f null -` OK. `GET /stream/stream.m3u8` 200 and `GET /stream/seg_000NN.ts` 200. |
| `/input/type` reaches window | **PASS** | `{"events":30,"ok":true}`; the xterm's `cat` file contained `ozma-live-1234`. |
| `/input/key` reaches window | **PASS** | evdev keycode 48 down/up -> `{"ok":true,"sink":"uinput"}`; file then contained `b`; final `od` showed `ozma-live-1234\nb\n`. |
| `/clipboard` round-trip | **FAIL** | Write returned `502 clipboard.write_failed` ("xclip timed out after 5000 ms"), yet a subsequent `GET /clipboard` and a direct `xclip -o` both returned the written text. An orphan `xclip -selection clipboard -i` (PPID 1) remained. NUL input correctly returned typed 400. |
| `/display/geometry` get | **PASS** | Parsed `DUMMY0` 800x600+0+0, primary, mode `800x600@60.32`, plus 23 advertised modes and the Python-shaped `screens` list. |
| `/display/geometry` set + restore | **PASS** | Idempotent set -> `changed:false`; set to `1024x768` -> `changed:true`, `applied 1024x768@60.0`, `xrandr --query` current 1024x768; restore to `800x600` -> `applied 800x600@60.32`, current 800x600. No rollback needed. |
| `/wallpaper` set + visual check | **PASS (with ambient DISPLAY)** | `POST {"path":".../wall.png"}` -> `applied:true`, `tool:"feh"`; root screenshot pixel `(700,500)` and `(10,590)` became `srgb(0,128,255)`. URL rejected 400 `wallpaper.remote_unsupported`. |
| Audio apply | **PASS** | `POST /audio/apply` -> 200; `pactl get-sink-volume ozma_codex_test` -> 35% both channels, unmuted. |
| Audio play | **PASS** | `POST /audio/play {"track":"tone.wav"}` -> 200; a sink-input appeared on `ozma_codex_test` and `pw-play --target ozma_codex_test .../tone.wav` was running. `/audio/stop` returned 200 and removed the sink-input/player. |
| Graceful SIGTERM | **PASS** | Node exited in 102 ms; log `shutdown requested; stopping node tasks signal="SIGTERM"`; both capture and HLS ffmpeg children gone; no listeners on TCP 17482 or UDP 17483/17484; `ozma-keyboard`/`ozma-mouse` removed from `/proc/bus/input/devices` and `xinput list`. |
| Explicit `--x11-display` for controls | **FAIL** | With `DISPLAY` unset and `--x11-display :90`, `desktop_controls` still advertised clipboard/wallpaper available; geometry worked, but `POST /clipboard` -> 503 `desktop.no_display` (`Can't open display: (null)`) and `POST /wallpaper` -> 502 (`feh ERROR: Can't open X display`). `GET /wallpaper` still returned 200. |

## Bugs found

### 1. `/clipboard` write always times out and leaks `xclip` (fixed in follow-up)

Observed response:

```json
{"code":"clipboard.write_failed",
 "details":{"tool":"xclip"},
 "error":"Could not run xclip: xclip timed out after 5000 ms",
 "ok":false,"retryable":false}
```

The write still took effect: `GET /clipboard` and
`DISPLAY=:90 xclip -selection clipboard -o` both returned the submitted text.
`ps` then showed `xclip -selection clipboard -i` with PPID 1, and it survived
node SIGTERM.

Cause: `xclip -i` forks a background selection owner that inherits the
stdout/stderr pipes. `SystemCommandRunner::run` waits for pipe EOF as well as
the direct child's exit, so it never observes completion; `kill_on_drop` only
targets the already-exited parent, leaving the grandchild orphaned. This affects
every real X11 clipboard write on a host with `xclip`, while the fake-runner
unit tests cannot see it.

Suggested fix: treat direct-child exit as command completion (drain/close the
inherited pipes with a short bounded grace, or use `Stdio::null()` for a
write-only command), then add a real `xclip` live test or a fake command that
forks a pipe-holding grandchild. Do not simply add `-loops 1`; that changes
clipboard ownership lifetime.

### 2. Explicit `--x11-display` is not passed to clipboard/wallpaper tools (fixed in follow-up)

`SystemCommandRunner` passes no `DISPLAY` environment to `xclip`/`feh`, and
their argv has no `--display`/`-display` element. Only `xrandr` receives
`--display`. With `DISPLAY` unset:

- `GET /api/v1/connection.desktop_controls` advertised `x11_display: ":90"`
  and `clipboard.available: true`, `wallpaper.available: true`;
- `GET /display/geometry` returned 200 for `:90`;
- `POST /clipboard` returned 503 `desktop.no_display` with
  `Error: Can't open display: (null)`;
- `POST /wallpaper` returned 502 `wallpaper.apply_failed` with
  `feh ERROR: Can't open X display`;
- `GET /wallpaper` still returned 200.

This is both a false availability claim and a safety problem: if ambient
`DISPLAY` points at another seat, clipboard and wallpaper would be applied to
that display instead of the configured `--x11-display`. Suggested fix: carry the
resolved display into `CommandRequest` and set `DISPLAY` (and
`WAYLAND_DISPLAY` for `swaybg`) for the child, and map feh's no-display error to
the typed 503 `desktop.no_display` already used for `xclip`.

### Minor observations (fixed in follow-up)

- `GET /wallpaper` returned `"display": null` and `"last_wallpaper": null`
  while `last_applied` was correct. The docs' example shows a resolved display
  and `verified:false`; the response shape differs. It now returns both the
  resolved `display` and `last_wallpaper` (and keeps the older field names).
- `feh --bg-fill` writes `~/.fehbg`. The test's file was created only by this
  run and was removed during cleanup. The node now passes `--no-fehbg`; a live
  check with a private `HOME` left no `.fehbg` behind.

## Cleanup

- Node (main and explicit-display instances) stopped with SIGTERM; no
  `ozma-node`, capture/HLS `ffmpeg`, or listener remained.
- Throwaway Xorg `:90` and `xterm` stopped; the private mount namespace ended
  with its Xorg process.
- Disposable PipeWire null sink `ozma_codex_test` unloaded; the reference tone
  and all node/HLS/wallpaper/screenshot files under
  `/tmp/opencode/ozma-desktop-live` removed.
- Orphan `xclip` from bug 1 killed during cleanup; `~/.fehbg` (created by this
  run) removed.
- No udev, group, permission, service, or default-audio change was made; no
  product code changed; no commit created.
