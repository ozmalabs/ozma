# Phase 4 desktop compatibility: geometry, clipboard, wallpaper

Status: **implemented for the `ozma-node --profile desktop` X11 seat.** The
node now exposes controller-facing display geometry, clipboard, and wallpaper
endpoints that map to real X11 tools, with typed unavailability when a tool or
display is absent. No endpoint fabricates a change: a second `xrandr --query`
verifies geometry, and clipboard/wallpaper success requires a real tool exit
status of zero.

This closes the desktop items listed in `docs/phase4-cutover-audit.md` for
display geometry/resolution, clipboard, and wallpaper. It does not claim the
remaining agent features: room-correction sweep, backup/storage, app monitoring,
tray/installer, or Windows/macOS.

## Routes

All routes are mounted in `compat_router`, i.e. behind the same node bearer
guard as `/input/*`, `/power/*`, `/audio/*`, `/codec`, `/usb/*`, and
`/display/select`. A node started with `--api-token` therefore answers
`401 Unauthorized` without the token on every one of these routes.

| Route | Behavior |
|---|---|
| `GET /display/geometry` | Runs `xrandr --display <D> --query`, parses `Screen N:` and every output (connection, primary, active mode/position/rotation, advertised modes). Returns both rich `outputs` and Python-agent-shaped `screens`. Typed 503 with no display or no `xrandr`. |
| `POST /display/geometry` | Body `{"output":"DP-1","mode":"1920x1080@60.00","x":0,"y":0,"rotation":"normal"}`. Validates output/connected/mode/position/rotation against the query, applies exactly one `xrandr --display <D> --output ... --mode ... [--rate ...] --pos XxY --rotate R`, re-queries, and reports `applied` only when the read-back matches. |
| `GET /clipboard` | Reads UTF-8 text with `xclip -selection clipboard -o` or `xsel -b -o`, bounded to 64 KiB. NUL/non-UTF-8 content is a typed 400; oversize content is a typed 413. |
| `POST /clipboard` | Body `{"text":"..."}`. Writes stdin to `xclip -selection clipboard -i` or `xsel -b -i`. Success requires exit 0; NUL/oversize input is rejected first. |
| `GET /wallpaper` | Reports the tool that would be used, the session displays, and the last path this node applied. `verified:false` because `feh`/`xwallpaper`/`swaybg` have no portable read-back command. Typed 503 when no wallpaper tool/session exists. |
| `POST /wallpaper` | Body `{"path":"/abs/local/image.png"}`. Rejects URLs, relative paths, non-regular files, empty files, files over 64 MiB, and non-PNG/JPEG/WebP/BMP/GIF magic. Applies `feh --bg-fill`, `xwallpaper --zoom`, or `swaybg -i ... -m fill`; `last_applied` changes only after the tool accepts it. |

### Geometry apply and rollback

`POST /display/geometry` accepts:

* an exact advertised mode label (`1920x1080@60.00`),
* an advertised mode name when exactly one refresh matches (`1280x720`),
* `WxH` when exactly one advertised mode has those dimensions,
* `WxH@refresh` (for example `1920x1080@50`).

An advertised name with several refresh rates is rejected with the available
labels instead of silently picking one. Position defaults to the current
position; rotation defaults to the current rotation.

On apply failure, or when `xrandr` exits zero but the follow-up query does not
show the requested mode/position/rotation, the node runs the previous state
back (`--mode`, optional `--rate`, `--pos`, `--rotate`) and re-queries. The error
document carries `rollback_attempted`, `rolled_back`, `rollback_argv`, and
`rollback_error`; there is no success-shaped `applied` field. If the output had
no active mode before the request there is nothing safe to restore, and the
report says so rather than turning the output off.

## Availability policy

Availability is advertised in `/api/v1/connection.desktop_controls` and
`/node/status` (the same document) without running a control command:

```json
"desktop_controls": {
  "x11_display": ":0",
  "wayland_display": null,
  "geometry": {"available": true, "tool": "xrandr", "tools": ["xrandr"]},
  "clipboard": {"available": true, "tool": "xclip", "tools": ["xclip"]},
  "wallpaper": {"available": true, "tool": "feh", "tools": ["feh"]},
  "last_wallpaper": null,
  "wallpaper_readback": false
}
```

* The X11 display comes from `--x11-display`, then ambient `DISPLAY`. Empty
  values are ignored. The node never scans X servers.
* `geometry` needs a display and `xrandr`.
* `clipboard` needs a display and `xclip` or `xsel`.
* `wallpaper` needs an X display with `feh`/`xwallpaper`, or
  `WAYLAND_DISPLAY` with `swaybg`.
* Looking up a tool in `PATH` is not running it. `GET` routes return typed 503
  when the advertised control is unavailable.
* Other profiles keep the routes registered but answer a typed 501
  (`desktop.not_enabled`) because no desktop controls are attached.

Typed failure codes:

| Code | Status | Meaning |
|---|---|---|
| `desktop.not_enabled` | 501 | Not a desktop-profile node. |
| `desktop.no_display` | 503 | No display configured; `xrandr`/clipboard says the display cannot be opened. |
| `desktop.tool_missing` | 503 | `details.control` and `details.candidates` name the missing tool set. |
| `display.geometry_query_failed` | 503 | Query timed out/was unparseable. |
| `display.mode_rejected` | 400 | Unknown output, disconnected output, no modes, bad rotation/position, or ambiguous/unadvertised mode. |
| `display.geometry_apply_failed` | 502 | Tool failed or read-back mismatched; rollback detail included. |
| `clipboard.read_failed`, `clipboard.write_failed` | 502 | Tool exited non-zero. |
| `clipboard.too_large` | 413 | Over 64 KiB. |
| `clipboard.encoding_unsupported` | 400 | NUL or invalid UTF-8. |
| `wallpaper.invalid_path` | 400 | Empty/relative/missing/non-file path. |
| `wallpaper.remote_unsupported` | 400 | URL given; no network fetch exists. |
| `wallpaper.format_unsupported` | 400 | Magic is not PNG/JPEG/WebP/BMP/GIF. |
| `wallpaper.too_large` | 413 | Over 64 MiB. |
| `wallpaper.apply_failed` | 502 | Tool failed or exited before `swaybg` became resident. |

## Safety and honesty

* No command is built from shell text. Every invocation is an exact argv vector
  passed to the injectable runner.
* Commands have a 5 s deadline and bounded stdout/stderr. `kill_on_drop` reaps
  timed-out children.
* Clipboard input/output is capped at 64 KiB and restricted to UTF-8 text
  without NUL bytes.
* Wallpaper input is a local regular file, format-checked by magic, capped at
  64 MiB, and passed as one argv element. There is no download path.
* `swaybg` is the only resident tool. It is observed for 750 ms; a process that
  stays alive is reported as started and is replaced (previous handle killed)
  on the next wallpaper change. A process that exits non-zero is a failure.
* `last_wallpaper` is labelled as the last value this node set, not a claim
  about the compositor's current wallpaper (`verified:false`).
* A geometry change reports `applied` only after a second query matches. On any
  failure the response is an error with rollback information.

## Tests

Default tests use a fake command runner and a fake display name; they never run
`xrandr`, `xclip`, `xsel`, `feh`, `xwallpaper`, or `swaybg`, and never touch the
real desktop.

Covered in `ozma-node/src/desktop_compat.rs`:

* exact query/apply/rollback argv,
* output/mode/primary/rotation parsing, including multiple refresh rates,
* mode validation (unknown output, disconnected output, unadvertised and
  ambiguous modes, bad rotation),
* rollback after a non-zero `xrandr` and after a zero-exit read-back mismatch,
* typed missing-tool and missing-display 503s,
* clipboard exact argv, 64 KiB bound, invalid UTF-8, NUL, and non-zero exit,
* wallpaper URL/relative/missing/directory/oversize/bad-magic rejection,
* `feh` failure does not update `last_applied`; `swaybg` staying resident is a
  successful start,
* status advertisement without invoking the runner,
* route success/501 shapes and the bearer guard (added to the existing
  `controller_compat` guard test).

Commands run:

```bash
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/desktop_compat.rs \
  ozma-node/src/controller_compat.rs ozma-node/src/main.rs ozma-node/src/lib.rs
```

## Live evidence

The ignored test starts a throwaway `Xvfb` on a private display (`:90`-`:139`),
queries geometry through the real `SystemCommandRunner`, applies the current
mode/position/rotation (an idempotent set), and confirms an unadvertised mode is
rejected. It never attaches to the operator's desktop.

```bash
OZMA_DESKTOP_LIVE=1 cargo test -p ozma-node --lib \
  live_xvfb_geometry_query_and_idempotent_set -- --ignored --nocapture
```

Result on the development host: `1 passed`. The host had `xrandr` and `Xvfb`
but no `xclip`, `xsel`, `feh`, or `xwallpaper`; those live paths therefore were
not exercised and remain covered by the fake-runner tests. `swaybg` was present
but there was no throwaway Wayland compositor, so no live wallpaper change was
attempted.

## Remaining desktop gaps

* Clipboard is X11-only (`xclip`/`xsel`). A Wayland-only seat without XWayland
  gets a typed 503; `wl-copy`/`wl-paste` are not wired.
* Wallpaper read-back is unavailable, so `GET /wallpaper` reports the last path
  this node set rather than the compositor's current state.
* GNOME/KDE/XFCE native wallpaper APIs (`gsettings`, Plasma D-Bus,
  `xfconf-query`) are not used; only `feh`, `xwallpaper`, and `swaybg`.
* Resolution/rotation changes target one output per request. Multi-output
  layout planning, mirroring, and scale/transform are not exposed.
* Room-correction `POST /audio/sweep`, backup/storage, app monitoring, chassis
  database, tray/installer, and Windows/macOS remain outside this work.
