# Phase 4 — real Xorg per-seat display/input isolation report

Date: 2026-11-25
Scope: `ozma-multiseat` (`seat_display`, `input_targeting`, one opt-in live
test, this report, and the three existing multiseat docs). No commit was made.
`ozma-node`, `ozma-usb`, `ozma-mesh`, and the controller Python were not
touched.

## Result

Two real Xorg servers on throwaway displays `:90` and `:91`, each with dummy
video, libinput, and its own `Xauthority`, enumerated four helper-created
uinput devices. `ProcessSeatInputTargeter` (real `xinput`) left only seat A's
devices enabled on `:90` and only seat B's on `:91`. A key injected through
seat A's source reached `xev -root` only on `:90`; a key through seat B reached
only `:91`. This closes the Xvfb-only evidence gap: Xvfb exposes only
core/XTEST devices, while Xorg + libinput exposes the helper's nodes.

## Host probe and packages

Before installing:

```text
$ command -v Xvfb Xorg xinput xauth xev Xephyr
/usr/bin/Xvfb
/usr/bin/Xorg
/usr/bin/xinput
/usr/bin/xauth
# xev: missing
# Xephyr: missing

$ ls /usr/lib/xorg/modules/drivers/
amdgpu_drv.so  modesetting_drv.so  wacom_drv.so
inputtest_drv.so  libinput_drv.so
# dummy_drv.so: missing

$ Xorg -version
/usr/lib/Xorg.wrap: Only console users are allowed to run the X server
```

Already installed: `xorg-server 21.1.26-1.1`, `xorg-server-xvfb
21.1.26-1.1`, `xorg-xinput 1.6.4-2.1`, `xorg-xauth 1.1.5-1.1`,
`xf86-input-libinput 1.5.0-1.2`, `/dev/uinput` (ACL grants the `matt` user
read/write). Only the dummy video driver and `xev` were missing, so the minimal
install was:

```bash
sudo pacman -S --needed --noconfirm xf86-video-dummy xorg-xev
# installed: xf86-video-dummy 0.4.1-3.1, xorg-xev 1.2.7-1.1
```

Package decision: both packages were newly installed. They are small
(`~0.06 MiB` installed), do not change the running desktop, and are required by
the opt-in live test. They were **kept** so the validation is reproducible;
nothing was uninstalled.

## Xorg setup

Two throwaway Xorg instances were started directly as root, bypassing the
distro wrapper that allows only console users. No physical GPU, output, VT
switch, or physical input device was touched.

```bash
sudo -n /usr/lib/Xorg :90 \
  -config /tmp/ozma-xorg-seat/display-90/xorg.conf \
  -logfile /tmp/ozma-xorg-seat/display-90/Xorg.log \
  -auth /tmp/ozma-xorg-seat/display-90/Xauthority \
  -nolisten tcp -noreset -novtswitch -sharevts -keeptty
# same command with :91 and display-91 paths
```

Each config used only the dummy driver and explicit libinput devices, with
udev auto-add disabled:

```ini
Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
    Option "AllowMouseOpenFail" "true"
EndSection

Section "Device"
    Identifier "ozma-dummy"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Monitor"
    Identifier "ozma-monitor"
    HorizSync 5.0-1000.0
    VertRefresh 5.0-200.0
    Modeline "1280x800" 83.50 1280 1352 1480 1680 800 803 809 831 -hsync +vsync
EndSection

Section "Screen"
    Identifier "ozma-screen"
    Device "ozma-dummy"
    Monitor "ozma-monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1280x800"
    EndSubSection
EndSection

Section "ServerLayout"
    Identifier "ozma-layout"
    Screen "ozma-screen"
    InputDevice "ozma-seat-seat-a-kbd-event22" "CoreKeyboard"
    InputDevice "ozma-seat-seat-a-mouse-event23" "CorePointer"
    InputDevice "ozma-seat-seat-b-kbd-event24" "SendCoreEvents"
    InputDevice "ozma-seat-seat-b-mouse-event25" "SendCoreEvents"
EndSection

Section "InputDevice"
    Identifier "ozma-seat-seat-a-kbd-event22"
    Driver "libinput"
    Option "Device" "/dev/input/event26"
    Option "AccelProfile" "flat"
EndSection
# ... one InputDevice section for each helper uinput device ...
```

Per-display auth used a random 16-byte `MIT-MAGIC-COOKIE-1`:

```bash
xauth -f /tmp/ozma-xorg-seat/display-90/Xauthority \
  add :90 MIT-MAGIC-COOKIE-1 <32-hex-chars>
chmod 600 /tmp/ozma-xorg-seat/display-90/Xauthority
```

## uinput devices and helper

To avoid physical input, the live test creates four throwaway uinput source
devices in-process (A keyboard, A mouse, B keyboard, B mouse, vendor
`0x1209`). The real helper then grabs and relays them:

```json
{"op":"hello","protocol":1}
{"op":"grab","seat_id":"seat-a","devices":[
  {"path":"/dev/input/event22","kind":"keyboard"},
  {"path":"/dev/input/event23","kind":"mouse"}]}
{"op":"grab","seat_id":"seat-b","devices":[
  {"path":"/dev/input/event24","kind":"keyboard"},
  {"path":"/dev/input/event25","kind":"mouse"}]}
```

Observed helper response:

```text
seat A uinput=[ozma-seat-seat-a-kbd-event22, ozma-seat-seat-a-mouse-event23]
seat B uinput=[ozma-seat-seat-b-kbd-event24, ozma-seat-seat-b-mouse-event25]
```

The helper-created nodes landed on `/dev/input/event26..29`. Both Xorg servers
were started after creation, so each enumerated all four:

```text
:90 and :91 xinput list --short
  ozma-seat-seat-a-kbd-event22
  ozma-seat-seat-a-mouse-event23
  ozma-seat-seat-b-kbd-event24
  ozma-seat-seat-b-mouse-event25
```

Critical integration detail: an explicit Xorg `InputDevice` **Identifier is
the XInput device name**. Using a friendly section name (for example
`ozma-seat-a-kbd`) makes `xinput` report that name, and the targeter correctly
fails to find the helper's `ozma-seat-...-eventN` name. The config identifier
must be the helper uinput name. The targeter error now points at this rule.

## Per-display isolation evidence

`ProcessSeatInputTargeter` applied with the real `xinput`:

```text
:90 targeting=SeatInputTargeting {
  seat_id: "seat-a", display: ":90",
  enabled: ["ozma-seat-seat-a-kbd-event22", "ozma-seat-seat-a-mouse-event23"],
  disabled: ["ozma-seat-seat-b-kbd-event24", "ozma-seat-seat-b-mouse-event25"] }

:91 targeting=SeatInputTargeting {
  seat_id: "seat-b", display: ":91",
  enabled: ["ozma-seat-seat-b-kbd-event24", "ozma-seat-seat-b-mouse-event25"],
  disabled: ["ozma-seat-seat-a-kbd-event22", "ozma-seat-seat-a-mouse-event23"] }
```

`xinput list-props <id>` on each display confirmed `Device Enabled: 1` for the
seat's own keyboard/mouse and `0` for the other seat's. `xinput list --short`
showed disabled devices as `[floating slave]`.

Event routing was measured with `stdbuf -oL xev -root -event keyboard` on both
displays. Injecting `KEY_A` through source A produced:

```text
:90  KeyPress ... keysym 0x61, a
:90  KeyRelease ... keysym 0x61, a
:91  (no seat A key)
```

Injecting `KEY_B` through source B produced the mirror:

```text
:91  KeyPress ... keysym 0x62, b
:91  KeyRelease ... keysym 0x62, b
:90  (no seat B key)
```

Release then re-enabled the other seat's devices on both displays:

```text
:90 release reenabled [B kbd, B mouse]
:91 release reenabled [A kbd, A mouse]
```

## Code changes

- `seat_display.rs` gained `SeatDisplayBackend::Xorg`, `SeatDisplayPrograms.xorg`,
  `SeatDisplayOptions.xorg_config`, and Xorg argv construction:
  `:N -config <file> -logfile <file> -nolisten tcp -noreset -novtswitch
  -sharevts -keeptty -auth <Xauthority>` (or `-ac`). Xorg geometry comes from
  the config. `Xorg` without a config is a typed `Unsupported` error, never an
  implicit host GPU probe.
- `main.rs` exposes `--seat-display xorg` and
  `--seat-display-xorg-config PATH`; validation requires the config for Xorg and
  rejects it for other backends.
- `input_targeting.rs` now names the identifier/name rule in the missing-device
  error.
- `tests/live_xorg_seat_smoke.rs` is the opt-in live test. It requires
  `OZMA_MULTISEAT_REAL_XORG=1`, passwordless `sudo -n`, the dummy driver,
  libinput, `xinput`, `xauth`, `xev`, `stdbuf`, and `/dev/uinput`.

## Blockers and gotchas

1. **Xorg wrapper privilege.** `/usr/bin/Xorg` execs `Xorg.wrap`, which rejects
   non-console users with `Only console users are allowed to run the X server`.
   The live test uses `sudo -n /usr/lib/Xorg`; production needs a privileged
   launcher or a wrapper with equivalent policy. The backend does not
   auto-escalate.
2. **Input device names.** As above, explicit Xorg `InputDevice` identifiers
   must equal the helper uinput names. A generated config that uses friendly
   names will break targeting; the typed error now explains this.
3. **uinput/udev timing.** A brand-new uinput event node is initially
   `root:root 0600`; udev applies the `input` group/uaccess ACL a moment later.
   The helper is unprivileged, so the live test waits for the node to become
   readable before grabbing. A production helper must run after udev settlement
   or with the documented device permissions.
4. **VT handling.** Two servers were started with `-novtswitch -sharevts
   -keeptty`; no VT was switched and the real Xorg on the host was untouched.
   Without these flags a seat server may switch or share the console
   unexpectedly.
5. **Xorg config generation is still operator-supplied.** The backend accepts a
   config; it does not generate dummy/monitor/libinput sections or discover the
   helper's new event numbers. That remains a documented integration gap.

## Cleanup

The live test owns all resources. Post-run checks:

```text
$ ls /tmp/.X11-unix/ | grep -E 'X9[01]'
# no output
$ ls /tmp/.X9[01]-lock
# no such file
$ pgrep -af 'Xorg :9[01]'
# no output
$ pgrep -af ozma-seat-input
# no output
$ cat /sys/class/input/event2{2,3,4,5}/device/name
# no such file: throwaway source uinput devices are gone
```

The test sends `shutdown` to the helper (releasing grabs and destroying its
uinput nodes), drops the source `VirtualDevice`s, terminates both Xorg servers
(SIGTERM, then SIGKILL as a bounded fallback), and asserts the display sockets
and locks are gone. Auth/config/log files live under a `tempfile::TempDir`,
which is removed on success and while unwinding after a panic. The manual probe
used the same `:90`/`:91` paths under `/tmp/ozma-xorg-seat`; that directory was
also removed. Neither run left a socket, lock, process, helper, or Xorg temp
directory behind.

## Remaining compositor gaps

- The validated seat is a **virtual Xorg root**, not a physical monitor. There
  is no DRM scanout ownership, compositor launch, window manager, or
  physical-output handoff. A user on the real desktop still sees nothing from
  these seats.
- No **Wayland compositor seat**; libinput enumeration is proven for Xorg only.
- No **per-seat session manager, cgroup, namespace, or login seat**. The helper
  and Xorg run outside systemd-logind seat policy (`systemd-logind: PID ...
  does not belong to any known session` was logged; harmless for the dummy
  server, but not a production seat).
- No **automatic Xorg config generation** from helper uinput names, no udev
  rule/hotplug netlink handoff, and no privilege drop for the Xorg launcher.
- Physical input was deliberately **not** grabbed in this run. The helper's
  real `EVIOCGRAB`/relay path was exercised against throwaway uinput sources;
  physical-device permission and recovery behavior remains covered by the
  separate opt-in real-device test.
- `ozma-node`/`ozma-softnode` do not yet consume the private `DISPLAY` and the
  targeted uinput devices as one session.

## Verification

```text
cargo test -p ozma-multiseat
  180 passed, 0 failed, 12 ignored (incl. the opt-in live Xorg smoke)

OZMA_MULTISEAT_REAL_XORG=1 cargo test -p ozma-multiseat \
  --test live_xorg_seat_smoke -- --ignored --nocapture
  1 passed (3.3 s), including event routing and cleanup assertions

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```
