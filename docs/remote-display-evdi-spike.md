# Remote display M2 spike: Xorg + modesetting + EVDI + libevdi

**Status: NOT RUN.** This document is a procedure and a recording template.
Nothing below is a claim that the configuration has been tested in this
repository. Fill in the *Observed* column and the results section only after
running it.

This is the M2 gate for "remote system as an additional monitor" on Linux:
before `ozma-display` advertises EVDI support, a real host must prove that

1. Xorg with the `modesetting` driver adopts an EVDI connector as an
   **extended** output, and
2. `libevdi` delivers the connector's pixels to a userspace client.

`ozma-display` itself stays safe and dependency-free: it has an `EvdiProvider`
detection scaffold only. The actual driver integration belongs in a small
privileged helper that owns the libevdi event loop, because linking libevdi
needs `unsafe` FFI and DRM buffer handling.

---

## 1. Scope and non-goals

In scope for the spike:

- one EVDI connector created by a throwaway userspace harness;
- Xorg (`Xorg`/`Xwayland` is **not** enough), the `modesetting` driver, and a
  normal window manager;
- xrandr layout, mode selection, window placement, and an ordinary screen
  capture of the output;
- libevdi frame delivery and a mode-change notification.

Not in scope:

- Wayland compositor-native outputs (a later provider family);
- HDR, audio over the virtual display, damage-aware encoding;
- a kernel driver or a production libevdi binding;
- multi-output EVDI, suspend/resume, Secure Boot signing;
- claiming support for any distro/kernel not recorded below.

The fallback remains explicit: if Xorg does not adopt the connector, the
feature is labelled **mirror**, never "additional monitor".

---

## 2. Version record (fill this in)

Record the exact values. A pass on one kernel/driver pair is not support for
another.

| Item | Command | Observed |
|---|---|---|
| Distribution / release | `cat /etc/os-release` | |
| Kernel release | `uname -r` | |
| Kernel config EVT/DRM | `grep -E 'CONFIG_DRM(_EVDI)?=' /boot/config-$(uname -r)` | |
| EVDI module package | `dpkg -l 'evdi-dkms'` / `dnf info evdi` | |
| EVDI module version | `modinfo -F version evdi` | |
| EVDI source commit | package changelog or DKMS tree | |
| libevdi package | `dpkg -l 'libevdi*'` | |
| libevdi soname/path | `ldconfig -p \| grep libevdi` | |
| libevdi ABI header | `evdi.h` package version | |
| Xorg server | `Xorg -version` | |
| modesetting driver | `dpkg -l xserver-xorg-video-modesetting` (or in-server) | |
| Mesa / glamor | `glxinfo -B` | |
| Window manager / desktop | name and version | |
| Physical GPU + driver | `lspci -nnk \| grep -A3 VGA` | |
| DisplayLink manager present? | `systemctl status displaylink` | |
| Host hardware | CPU/GPU/RAM | |

Also record whether the machine uses Secure Boot (`mokutil --sb-state`) and
whether the module is signed or loaded with Secure Boot disabled.

---

## 3. Safety and rollback

- Prefer a disposable VM or a machine you can reboot. EVDI is an out-of-tree
  DRM module; a bad build or mismatched libevdi can crash Xorg or the kernel.
- Root commands below are labelled **`[ROOT]`**. The probe script is read-only
  by default and prints these commands instead of running them.
- Do not install EVDI on a host that already relies on the DisplayLink manager
  for production displays without recording both versions; they compete for
  the same module and device nodes.
- Rollback: stop Xorg, `[ROOT] modprobe -r evdi`, remove the DKMS package,
  restore the previous Xorg configuration or snapshot, reboot if the module
  cannot unload.
- Do not paste serial numbers, EDIDs from real monitors, or full `dmesg` into
  a public issue without reviewing them.

---

## 4. Prerequisites

### 4.1 Kernel module and userspace library

Debian/Ubuntu names are commonly `evdi-dkms` and `libevdi0` or `libevdi1`;
package names change over releases, so confirm with the distro before
installing.

```sh
# Probe only (no changes):
scripts/remote-display-spike.sh --probe

# [ROOT] Debian-family example. Review before running.
apt-get update
apt-get install -y evdi-dkms libevdi0
modprobe evdi

# [ROOT] Fedora-family example; package availability varies.
dnf install -y evdi
modprobe evdi
```

Verify:

```sh
lsmod | grep '^evdi'
ls -l /sys/module/evdi
ls -l /dev/dri
modinfo evdi
ldconfig -p | grep libevdi
```

Expected: `evdi` is loaded, `/dev/dri` contains at least one `card*` and one
`renderD*`, and a `libevdi.so.*` path is listed. If `/dev/dri` is absent, this
is not an Xorg problem; fix the DRM/module side first.

### 4.2 Xorg with modesetting

EVDI exposes a DRM device. Xorg must run with the `modesetting` driver, not a
vendor DDX that ignores secondary DRM devices.

```sh
# Confirm which server is running, not just which binary is installed.
Xorg -version
ps -ef | grep '[X]org'
grep -i 'modeset\|evdi' /var/log/Xorg.0.log
xrandr --listproviders
```

A minimal `xorg.conf.d` fragment that forces modesetting for the primary GPU
(adapt `Driver`/`BusID` to the machine; keep a backup):

```conf
Section "Device"
    Identifier "PrimaryGPU"
    Driver     "modesetting"
    Option     "PrimaryGPU" "true"
EndSection
```

Do not add an EVDI-specific `Device` section on the first attempt: EVDI is not
a normal PCI GPU and the server should discover it as a secondary DRM device.
Restart Xorg after changing the configuration and confirm the session still
starts before introducing EVDI.

---

## 5. Create a connector with libevdi

EVDI's interface is a C library. The harness is intentionally outside this
repository and outside `ozma-display` (which forbids `unsafe`). A throwaway C
program, or the `libevdi` examples shipped by the EVDI source tree, is enough.

The shape of the harness, with names from `evdi.h` (confirm signatures against
the installed header, because they have changed across versions):

1. `evdi_open(0)` (or `evdi_open_attached_to`) to obtain a handle.
2. Register an `evdi_event_context` with `evdi_register_event_context`.
3. Generate an EDID. `ozma-display` includes a 128-byte builder and a small
   dumper (raw bytes by default, `--hex` for inspection):

   ```sh
   cargo run -p ozma-display --example dump_edid -- \
     laptop-1 "Remote laptop" 1920 1200 60 > /tmp/edid.bin
   ```

   Pass those bytes to `evdi_connect(handle, edid, 128, 0)`. Start with
   1920x1200@60, then test 1920x1080@60.
4. On the mode-changed callback, read `struct evdi_mode` (width, height,
   refresh, bits per pixel, pixel format) and allocate a buffer.
5. `evdi_register_buffer(handle, buffer_id)`; on update-ready, call
   `evdi_request_update(handle, buffer_id)`.
6. Wait on the handle's event fd (`evdi_get_event_ready`), call
   `evdi_handle_events`, then `evdi_grab_pixels(handle, rects, &count)`.
7. Treat the buffer as the driver's pixel format (commonly 32-bit BGRA/XRGB).
   Record stride, byte order, and whether the buffer is bottom-up.
8. On DPMS/mode events, log the values instead of guessing.

Record the exact `evdi_mode` and the first frame's dimensions, stride, and
pixel format. A frame whose size or stride disagrees with the mode is a fail
even if the pixels look plausible.

`xrandr` should now list a new output (commonly named like `DVI-I-1-1` or
`DP-1-1`). Names are driver-version dependent; record the actual name.

---

## 6. Make Xorg adopt it as an extended output

```sh
xrandr --listproviders
# Example: provider 0 is the primary GPU, provider 1 is EVDI.
# Numbers are assigned by the server; read them, do not assume them.
xrandr --setprovideroutputsource 1 0

xrandr --query
xrandr --listmonitors

# Replace names/modes with the observed ones.
xrandr --output DVI-I-1-1 --mode 1920x1200 --right-of eDP-1 --auto
xrandr --listmonitors
```

Verification of **extended**, not mirrored:

- `xrandr --listmonitors` shows two monitors with different `+x+y` origins,
  and the EVDI monitor is not merely a clone of the primary.
- The EVDI output has a non-zero area to the right of (or below) the primary.
- The Xorg log records the modesetting output and does not fall back to a
  shadow/framebuffer-only path. Capture the relevant lines:

  ```sh
  grep -iE 'modeset|evdi|output .* connected|provider' /var/log/Xorg.0.log
  ```

- A window can be moved onto the EVDI output and is not clipped to the primary:

  ```sh
  # Any of these; install what the host already has.
  xdotool getdisplaygeometry
  wmctrl -lG
  xrandr --output DVI-I-1-1 --primary   # optional, to test primary handling
  ```

- A full-screen application on the EVDI output renders there (for example
  `mpv --fs --geometry=+2000+0 some-video`, `glxgears -geometry 800x600+2000+0`,
  or a game). Record whether GL/compositing works or only software rendering.
- An ordinary screen capture sees the output, independent of Ozma:

  ```sh
  # Whole root window; adjust DISPLAY if needed.
  import -window root /tmp/spike-root.png
  # Or a short capture with ffmpeg.
  ffmpeg -f x11grab -video_size 3840x1200 -i "$DISPLAY" -t 5 /tmp/spike.mp4
  ```

  The captured image must include content drawn on the EVDI area.

- Cross-check that libevdi's grabbed frame matches what Xorg shows: draw a
  known pattern or move a window, capture both the root window and the
  libevdi buffer, and compare visible regions. A black libevdi frame while
  Xorg shows content (or vice versa) is a failure of the path.

Mode change check:

```sh
xrandr --output DVI-I-1-1 --mode 1920x1080
# Then confirm the libevdi mode-changed callback fired and the harness
# reallocated to 1920x1080 without restarting Xorg.
```

---

## 7. Pass / partial / fail

### PASS (all required)

- `evdi` module loaded; `/dev/dri` and `libevdi` present.
- Xorg with modesetting starts and logs no EVDI-related fatal error.
- The connector appears in `xrandr --query` and is adopted by the primary
  provider via `--setprovideroutputsource`.
- It can be configured as a distinct extended monitor in
  `xrandr --listmonitors` with a non-zero offset.
- A window can be moved onto it, a full-screen app renders on it, and a plain
  `x11grab`/`import` capture includes its content.
- libevdi delivers frames with matching dimensions/format, and frame content
  visibly tracks the Xorg content.
- A mode change updates both Xorg and the libevdi callback without restarting
  the session.
- Ten minutes of use produces no Xorg crash, no kernel oops, and no runaway
  CPU/memory in the harness.

### PARTIAL (records a limitation, not a feature)

- Connector appears but Xorg will not adopt it (`setprovideroutputsource`
  fails or output stays disconnected/unknown).
- Xorg can only mirror, not extend.
- libevdi frames arrive but are all black or have wrong stride/format.
- GL apps work only in software, or compositing/window manager misbehaves.
- Mode changes need an Xorg restart.

A partial result means the provider must remain `supported: false`; record the
exact failing step and error text.

### FAIL / stop

- Module cannot load, or loading it destabilises the host.
- Kernel oops, GPU hang, or Xorg crash that recurs.
- No connector after `evdi_connect`, with the EDID accepted.
- Frames cannot be captured at all.
- Secure Boot/signing blocks the module and the host policy will not allow an
  exception.

Stop the spike and escalate before trying random kernel parameters.

---

## 8. Results record

```
Date:
Operator:
Git commit of ozma-display under test:
Result: PASS / PARTIAL / FAIL
EVDI module version:
libevdi version + path:
Xorg version + modesetting source:
Kernel:
Distro:
GPU + driver:
Connector name:
Mode(s) tested:
Extended or mirrored:
Window/full-screen test:
x11grab/import test:
libevdi frame format/stride:
Mode-change result:
Uptime/soak:
Errors and exact log excerpts:
```

Attach the probe output from `scripts/remote-display-spike.sh --probe`, the
relevant Xorg log lines, and one screenshot of the host's display settings and
one of the captured EVDI output. Redact serial numbers if the report is public.

---

## 9. What this unlocks in `ozma-display`

If and only if the spike passes on a recorded configuration:

- the `EvdiProvider` scaffold becomes a real provider behind a privileged
  helper; `capabilities().supported` can become `true` for that recorded
  matrix only;
- the helper must expose create/destroy, EDID bytes, mode notifications, DPMS,
  and a frame handle to the network-facing process without running the API as
  root;
- `supported: false` remains the answer everywhere else, and the UI keeps
  offering a labelled mirror.

Until then, the in-repo fake provider and the EDID/mapping tests define the
behaviour above the platform layer. No claim of EVDI support should be added to
capability JSON, docs, or release notes based on this document alone.
