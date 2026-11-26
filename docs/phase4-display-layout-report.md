# Phase 4 multi-output display layout

Status: **implemented for the `ozma-node --profile desktop` X11 seat.**
`GET /display/layout` reports every output and logical monitor.
`POST /display/layout` plans, validates, applies, verifies, and (on failure)
rolls back a multi-output layout with exact `xrandr` argv. The existing
single-output `/display/geometry` behavior is unchanged.

Implementation: `ozma-node/src/desktop_compat.rs`, mounted by
`controller_compat::compat_router` behind the node bearer guard.

## Routes

| Route | Behavior |
|---|---|
| `GET /display/layout` | Runs `xrandr --display <D> --query`, `xrandr --display <D> --verbose`, and `xrandr --display <D> --listmonitors`. Returns each output's connection/enabled/primary flag, active mode and refresh, mode size, position, rotation, scale/transform, physical size, and advertised modes, plus every logical monitor. |
| `POST /display/layout` | Body lists output changes and an optional logical-monitor list. Validates against the live query, applies one exact argv, re-queries, and returns `applied:true` only when the read-back matches. On any failure it restores the previous connected outputs and non-automatic monitors. |

Both routes answer a typed 503 when the profile has no X11 display or no
`xrandr`. Other profiles keep the route registered and answer a typed 501
(`desktop.not_enabled`).

## Request shape

```json
{
  "outputs": [
    {
      "output": "DP-1",
      "mode": "1920x1080@60.00",
      "x": 0,
      "y": 0,
      "rotation": "normal",
      "primary": true,
      "scale": "1x1"
    },
    {
      "output": "HDMI-1",
      "mode": "1920x1080@60.00",
      "same_as": "DP-1"
    },
    {
      "output": "HDMI-2",
      "enabled": false
    }
  ],
  "monitors": [
    {
      "name": "main",
      "width": 3840,
      "height": 1080,
      "mm_width": 1054,
      "mm_height": 296,
      "x": 0,
      "y": 0,
      "outputs": ["DP-1", "HDMI-1"]
    }
  ]
}
```

* `enabled` defaults to the current state unless another field explicitly asks
  for an active output. An explicit `false` may not be combined with `mode`,
  `x`, `y`, `rotation`, `scale`, `same_as`, or `primary:true`.
* `mode` accepts an advertised name, `name@refresh`, `WxH`, or `WxH@refresh`.
  Ambiguous names with several refresh rates are rejected with the available
  labels, exactly like `/display/geometry`.
* `position` defaults to the current position. `rotation` defaults to the
  current rotation.
* `same_as` mirrors another enabled output. The target must have the same mode
  size, refresh, rotation, and scale. The mirrored output must omit `x`/`y`
  because its position comes from the target. Mirror chains are rejected.
* `scale` is a bounded diagonal scale (`XxY`, or a single number for both axes)
  in `0.25..=4.0`. It is emitted as `--scale`; a non-diagonal transform read
  back from the server is preserved by rollback through `--transform`.
* `primary:true` may appear on at most one enabled output. If no output asks to
  be primary, the current primary is kept when it remains enabled. Explicitly
  setting the current primary to `false` with no replacement clears it with
  `--noprimary`.
* `monitors` absent/null leaves logical monitors untouched. `[]` removes the
  monitors this node previously created (tracked in memory). A non-empty list
  is authoritative for this node's monitors: stale tracked monitors are
  deleted and every listed monitor is set.

## Validation

The whole request is validated before any command runs:

* every output must be advertised; an enabled output must be connected;
* every requested mode must resolve to an advertised mode;
* positions are `0..=65535` and each output's scaled/rotated rectangle must fit
  inside the screen's advertised maximum (`Screen N: ... maximum WxH`);
* enabled output rectangles may not overlap, except an explicit `same_as`
  mirror pair;
* scale factors must be finite and inside `0.25..=4.0`;
* at most 16 output entries and 16 logical monitors;
* logical-monitor names are 1-64 bytes from `A-Z a-z 0-9 _ - . : @ /` and may
  not be `none` or start with `+`/`*`;
* logical monitors need a non-zero size, bounded position, and at least one
  enabled output;
* an output may belong to only one logical monitor, and every monitor output
  must exist and be enabled;
* logical monitors may not overlap unless their rectangles are identical
  (allowing an explicitly mirrored monitor group).

Rejections are typed `400 display.layout_rejected` (or
`display.mode_rejected` for an unadvertised/ambiguous mode). More than 16
outputs or monitors is `413 display.layout_too_large`.

## Exact argv and application

Output clauses are emitted in request order, followed by stale monitor
deletions and requested monitor definitions:

```text
xrandr --display :0 \
  --output DP-1 --mode 1920x1080 --rate 60.00 --pos 0x0 --rotate normal --primary \
  --output HDMI-1 --mode 1920x1080 --rate 60.00 --same-as DP-1 --rotate normal \
  --output HDMI-2 --off \
  --delmonitor old-name \
  --setmonitor main 3840/1054x1080/296+0+0 DP-1,HDMI-1
```

No shell is involved: the argv vector goes straight to the injectable
`CommandRunner`, with the same 5 s deadline and bounded stdout/stderr as the
other desktop controls. Monitor geometry is emitted as
`W/mmWxH/mmH+X+Y`, matching `xrandr --setmonitor`.

## Read-back and rollback

Success requires a fresh three-command read-back to match:

* every requested output's enabled state, mode size, refresh, position,
  rotation, primary flag, and 3x3 transform;
* every requested logical monitor's name, geometry, physical size, and output
  set;
* every tracked monitor named in `--delmonitor` to be gone.

`applied:true` and `ok:true` are only returned after that comparison. A success
document also includes the exact `argv`, the previous `LayoutState`, and the
read-back `layout`.

On a non-zero apply, an unreadable read-back, or a mismatch, the node:

1. restores every connected output from the previous state (`--off` for
   previously disabled outputs, `--mode`/`--rate`/`--pos`/`--rotate`,
   `--scale` or `--transform`, and `--primary`/`--noprimary`);
2. reads the current monitor list and deletes only node-created monitors that
   are not in the previous non-automatic set;
3. re-defines every previous non-automatic monitor with `--setmonitor`;
4. re-queries and compares the restored state.

The error is `502 display.layout_apply_failed` with no success-shaped
`applied`, and carries:

```json
{
  "rolled_back": true,
  "rollback_attempted": true,
  "rollback_argv": ["--display", ":0", "..."],
  "rollback_monitor_argv": [["--display", ":0", "--delmonitor", "main"]],
  "rollback_error": null,
  "requested": { "...": "..." }
}
```

If a monitor that was never created is named, the rollback only deletes
monitors actually present, so `xrandr --delmonitor missing` cannot abort the
output restore. Monitor removal and definition are separate invocations from
the output restore so one monitor failure does not prevent outputs returning to
their previous state.

## Availability and typed errors

`/api/v1/connection.desktop_controls` now advertises both `geometry` and
`layout`, each a `PATH` lookup for `xrandr` plus the resolved X display:

```json
"layout": {"available": true, "tool": "xrandr", "tools": ["xrandr"]}
```

| Code | Status | Meaning |
|---|---|---|
| `desktop.not_enabled` | 501 | Not a desktop-profile node. |
| `desktop.no_display` | 503 | No display configured, or `xrandr` cannot open it. |
| `desktop.tool_missing` | 503 | `xrandr` is absent (`details.control = "geometry"`). |
| `display.layout_query_failed` | 503 | `--query`, `--verbose`, or `--listmonitors` timed out/unparseable. |
| `display.layout_rejected` | 400 | Unknown/disabled output, overlap, bounds, scale, mirror, or monitor validation failure. |
| `display.mode_rejected` | 400 | Mode name/size/refresh is not advertised or is ambiguous. |
| `display.layout_too_large` | 413 | More than 16 outputs or 16 monitors. |
| `display.layout_apply_failed` | 502 | `xrandr` failed or the read-back mismatched; rollback detail included. |

## Tests

Default tests use the fake runner; they never run `xrandr` and never touch a
real display.

`ozma-node/src/desktop_compat.rs` covers:

* verbose transform/physical-size parsing, including `--scale 1.5x1.5`, and
  `--listmonitors` parsing for automatic, primary, and custom monitors;
* `GET` merging of query/verbose/listmonitors and exact query argv;
* exact multi-output apply argv with position, rotation, primary, `--off`,
  mirror `--same-as`, bounded `--scale`, and `--setmonitor`;
* full read-back verification (output fields, transform, monitors);
* validation of unknown output, unknown/ambiguous mode, scale bounds, output
  overlap, screen bounds, mirror size mismatch, logical-monitor overlap, and
  unknown monitor output;
* stale `--delmonitor` planning for a tracked monitor;
* rollback after a non-zero apply and after a zero-exit read-back mismatch,
  including exact previous-output argv and `rolled_back:true`;
* route success/501 shapes and the bearer-guard entry in
  `controller_compat::tests`.

Commands run:

```bash
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/desktop_compat.rs \
  ozma-node/src/controller_compat.rs
```

Results on the development host: `cargo test -p ozma-node --lib` ->
`819 passed; 0 failed; 10 ignored` (the ignored count includes the live tests
below); `cargo check -p ozma-node --all-targets` clean; rustfmt clean.

### Live evidence

```bash
OZMA_DESKTOP_LIVE=1 cargo test -p ozma-node --lib \
  live_xvfb_layout_query_and_idempotent_set -- --ignored --nocapture
```

`1 passed`. This starts a throwaway `Xvfb` on a private display, queries the
layout through `SystemCommandRunner`, confirms a logical monitor is reported,
applies the current mode/position/rotation/primary (idempotent), verifies the
read-back, and confirms an unadvertised mode is rejected. Xvfb's RandR
implementation does not persist custom monitor names, so `--setmonitor` is
covered separately:

```bash
OZMA_DESKTOP_LIVE=1 OZMA_LAYOUT_LIVE_DISPLAY=:98 cargo test -p ozma-node --lib \
  live_external_display_custom_monitor_round_trip -- --ignored --nocapture
```

`1 passed` against a throwaway Xorg `dummy` server on `:98`. The server was
started with `AutoAddDevices=false`/`AutoEnableDevices=false`, so it did not
attach real input devices or GPU outputs. The test defined `ozma-test` with
`--setmonitor`, verified it in `--listmonitors`, then removed it with
`--delmonitor`. The host's real desktop was never used. All throwaway X servers
and sockets were removed after the runs.

## Remaining gaps

* Layout is X11 `xrandr` only. A Wayland-only seat without XWayland gets the
  typed 503; no compositor-native layout protocol is used.
* Scaling is a bounded diagonal `--scale`. Arbitrary `--transform`, reflection,
  panning, gamma/brightness, and display-link/MST (`--setprovideroutputsource`)
  requests are not exposed. Read-back and rollback do preserve an existing full
  transform matrix.
* Logical monitors with overlapping but unequal rectangles are rejected.
  Identical rectangles (mirror groups) are allowed.
* Monitor ownership is tracked only in memory. After a node restart it does not
  know which non-automatic monitors it created, so `monitors:[]` does not remove
  monitors from an earlier process lifetime. Operator-created monitors are never
  deleted unless this process tracked them.
* Per-output `--rate` selection is accepted through the mode label, but there is
  no separate refresh field, and mode creation (`--newmode`/`--addmode`) is not
  exposed.
* The existing `/display/geometry` route remains single-output and intentionally
  unchanged; clients that need multiple outputs use `/display/layout`.
