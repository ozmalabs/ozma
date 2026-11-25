# Rust-first systemd assets

These units run the Rust node stack. The Python `softnode/`, `node/`, and
`agent/` trees are not referenced and remain available for rollback during the
one-release overlap.

| File | Purpose |
|---|---|
| `ozma-node@.service` | System template; `%i` is `hardware`, `vm`, or `desktop`. |
| `ozma-node-vm@.service` | Per-VM template; `%i` is the VM/node name (`--name %i`). |
| `ozma-virtual-node.service` | Hypervisor supervisor: discovers VMs and spawns one `ozma-node --profile vm` per VM. |
| `ozma-multiseat.service` | Desktop-seat supervisor: loads the persisted `seats.json` and supervises one `ozma-node --profile desktop` per X output. |
| `env/*.env.example` | Environment-file examples. Copy, do not edit in place. |

`ozma-meeting-detect` is installed as a binary but has **no unit**. It is a
per-session, on-demand IPC server (`$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`,
fallback `/tmp/ozma-meeting-detect.sock`) and is not yet wired to the controller
report path; start it in the session that owns the calendar data.

`@BIN_DIR@`, `@ENV_DIR@`, `@STATE_DIR@`, `@RUNTIME_DIR@`,
`@SUPERVISOR_STATE_DIR@`, `@SUPERVISOR_RUNTIME_DIR@`, `@OZMA_USER@`, and
`@OZMA_GROUP@` are substituted by `scripts/install-rust.sh` (and by
`scripts/install-system.sh`, which also renders the host assets). The defaults
in a system install are
`/usr/local/bin`, `/etc/ozma`, `/var/lib/ozma-node`, `/run/ozma-node`,
`/var/lib/ozma`, and `/run/ozma`.

## Flags vs environment variables

Every `--flag` with an `env = "OZMA_..."` attribute can be set from an env file.
Flags without one (`--hls`, `--capture-provider`, `--display-output`,
`--usb-gadget`, `--qmp-dir`, and the supervisor's `--provision*`) must go in
`OZMA_NODE_ARGS` / `OZMA_VIRTUAL_NODE_ARGS`. systemd splits those variables at
whitespace, so do not quote individual values. Keep them on one line.

## Install (no sudo)

```bash
scripts/install-rust.sh --build --prefix "$HOME/.local"
```

This installs `ozma-node`/`ozma-virtual-node` (and `ozma-multiseat` plus
`ozma-meeting-detect` when their binaries are built) under `~/.local/bin`, lays
out user units under `~/.local/lib/systemd/user`, and writes env examples under
`~/.local/etc/ozma`. Use `--no-multiseat`/`--no-meeting-detect` to skip the
optional binaries and the multiseat unit. It never removes Python and never
starts a unit. Then:

```bash
systemctl --user daemon-reload
systemctl --user link ~/.local/lib/systemd/user/ozma-node@vm.service
# fill in ~/.local/etc/ozma/ozma-node-vm.env first
systemctl --user enable --now ozma-node@vm.service
```

## System install (operator, requires root)

`scripts/install-system.sh` is the scripted path for the host-level pieces
(user/group, directories, udev, AppArmor, tmpfiles/modules-load, and the
copy-only libvirt snippet). It also renders these units/env examples, never
enables or starts a unit, and never edits `/etc/libvirt/qemu.conf`. See
[`docs/clean-host.md`](../../docs/clean-host.md#system-install-fresh-host) for
the full fresh-host flow and rollback.

The equivalent manual sequence, if you prefer to run each step yourself, is:

```bash
sudo groupadd --system ozma-node
sudo useradd --system --gid ozma-node --home /var/lib/ozma-node \
    --shell /usr/sbin/nologin --no-create-home ozma-node
sudo usermod -aG video,input,render ozma-node    # existing groups only
sudo install -d -m 0750 -o ozma-node -g ozma-node \
    /var/lib/ozma-node /run/ozma-node /run/ozma/qmp
sudo scripts/install-rust.sh --from-target target/release --system --prefix /usr/local
# copy and edit /etc/ozma/*.env.example, then:
sudo systemctl daemon-reload
sudo systemctl enable --now ozma-node@hardware.service
# or, on a hypervisor:
sudo systemctl enable --now ozma-virtual-node.service
# or, for desktop seats (edit /etc/ozma/ozma-multiseat.env first):
sudo systemctl enable --now ozma-multiseat.service
```

The installer itself never uses sudo.

`install-rust.sh` does not create the system user or chown directories; that is
an administrative decision and is left explicit. `scripts/install-system.sh`
performs those steps (and the udev/AppArmor/libvirt-snippet work) when run as
root, or records them in `/etc/ozma/install-operator-commands.txt` under
`--root`/`--dry-run`.

## Least privilege

- Run as a dedicated `ozma-node` user. Never run the node as root: HID gadget
  and `/dev/uinput` access are granted by group/udev permissions, not root.
- Hardware profile: `video` group for `/dev/video*`; HID gadget access needs
  ConfigFS and USB gadget permissions from the host/package. Delete the
  `/dev/uinput` and `/dev/input/event*` rules if the profile does not inject
  local input.
- VM profile: `input` group for uinput/evdev, and read/write access to the QMP
  sockets. The node does not need `/dev/kvm`, libvirt, or QEMU process access.
- `ozma-virtual-node`: add `libvirt`/`kvm` only when libvirt discovery or power
  is used. `--provision`, `--provision-restart`, and `--provision-evdev` edit
  host XML or devices and stay off by default. Proxmox configuration is never
  edited.
- Desktop profile: a system unit runs as `ozma-node` and cannot see a logged-in
  user's PipeWire session. Prefer a user unit (`systemctl --user`) or grant the
  desktop session explicitly. `ProtectHome=yes` is correct for system units and
  should be relaxed only for a user unit that must capture files under `$HOME`.
- `ozma-multiseat`: runs as the same account as the node units and spawns the
  seat children, so it needs the same device/group access. It keeps
  `PrivateTmp=no` because X11 sockets live in `/tmp/.X11-unix`. A system unit
  cannot read a user's `~/.Xauthority` under `ProtectHome=yes`; either use a
  user unit or point `XAUTHORITY` at a readable host path. Leave
  `--print-plan`/`--dry-run` out of the unit (they print and exit). Seat
  ownership/routing/audio/private-display/input-isolation modes stay opt-in
  through `OZMA_MULTISEAT_ARGS`.
- Keep `ProtectSystem=strict`, `NoNewPrivileges=yes`, and the restricted address
  families. Add only the `DeviceAllow=` entries the selected profile needs.
- Put `OZMA_API_TOKEN`/`OZMA_NODE_API_TOKEN` in the env file with mode `0600`;
  they are never written to the unit or logs.

## Validate units

```bash
systemd-analyze verify scripts/systemd/ozma-node@.service \
    scripts/systemd/ozma-node-vm@.service \
    scripts/systemd/ozma-virtual-node.service \
    scripts/systemd/ozma-multiseat.service
```

`scripts/systemd/verify-units.sh` renders every unit (system and least-privilege
user) to a temp directory first, so it needs neither installed binaries nor the
service account.

`systemd-analyze` is not required on the build host; absence is reported as a
SKIP by `scripts/clean-host/check-deps.sh`.

## Host-level udev/AppArmor/libvirt configuration

`scripts/install-system.sh` and the assets under `scripts/system/` now own the
Rust-side udev rules, AppArmor libvirt-qemu drop-in, and libvirt
`cgroup_device_acl` snippet. The Python `softnode/install/install.sh` is still
present for the one-release rollback and remains compatible with the same
`/dev/input/event*` and uinput paths, but it is no longer the only owner of
these steps. The libvirt `qemu.conf` merge stays an explicit operator action;
the installer only writes a snippet under `/etc/ozma/`.
