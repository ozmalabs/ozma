# System install assets

These are the host-level pieces installed by `scripts/install-system.sh` for a
system-wide Rust node deployment. They complement the user/system unit
templates in `scripts/systemd/`, which the same installer also renders.

| Asset | Installed to | Purpose |
|---|---|---|
| `udev/70-ozma-node-uinput.rules.in` | `/etc/udev/rules.d/` | `/dev/uinput` access for vm/desktop input. |
| `udev/70-ozma-node-input.rules.in` | `/etc/udev/rules.d/` | `/dev/input/event*` and virtual `ozma-*` devices. |
| `udev/70-ozma-node-hidg.rules.in` | `/etc/udev/rules.d/` | `/dev/hidg*` for the hardware HID gadget. |
| `udev/70-ozma-node-v4l2.rules.in` | `/etc/udev/rules.d/` | `/dev/video*` for hardware capture. |
| `udev/70-ozma-node-render.rules.in` | `/etc/udev/rules.d/` | `/dev/dri/renderD*` for hardware encode. |
| `udev/70-ozma-node-gadget.rules.in` | `/etc/udev/rules.d/` | Opt-in UDC/gadget class access (`--usb-gadget`). |
| `apparmor/libvirt-qemu.d-ozma` | `/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma` | Allow QEMU input-linux to read evdev nodes. |
| `libvirt/qemu-cgroup_device_acl.conf.snippet` | `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf` | Copy-only guidance; `qemu.conf` is never edited. |
| `tmpfiles/ozma-node.conf.in` | `/etc/tmpfiles.d/ozma-node.conf` | Recreate `/run/ozma*` directories at boot. |
| `modules-load/ozma-node.conf.in` | `/etc/modules-load.d/ozma-node.conf` | Load `uinput` (and `libcomposite` with `--usb-gadget`). |
| `systemd/10-usb-gadget.conf.in` | `<unit-dir>/ozma-node@.service.d/` | Opt-in ConfigFS write access for the gadget. |

The `.in` suffix marks a template rendered with the install paths, service user
and group, and the host's QMP group (`kvm` when present, otherwise `root`).

`scripts/install-system.sh` never enables or starts a unit, never edits
`/etc/libvirt/qemu.conf`, and never invokes `sudo`. `scripts/verify-system-install.sh`
renders every asset into a temporary root and validates it without root.
