# Rust-first systemd assets

These units run the Rust node stack. The Python `softnode/`, `node/`, and
`agent/` trees are not referenced and remain available for rollback during the
one-release overlap.

| File | Purpose |
|---|---|
| `ozma-node@.service` | System template; `%i` is `hardware`, `vm`, or `desktop`. |
| `ozma-node-vm@.service` | Per-VM template; `%i` is the VM/node name (`--name %i`). |
| `ozma-virtual-node.service` | Hypervisor supervisor: discovers VMs and spawns one `ozma-node --profile vm` per VM. |
| `env/*.env.example` | Environment-file examples. Copy, do not edit in place. |

`@BIN_DIR@`, `@ENV_DIR@`, `@STATE_DIR@`, `@RUNTIME_DIR@`,
`@SUPERVISOR_STATE_DIR@`, and `@SUPERVISOR_RUNTIME_DIR@` are substituted by
`scripts/install-rust.sh`. The defaults in a system install are
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

This installs `ozma-node`/`ozma-virtual-node` under `~/.local/bin`, lays out
user units under `~/.local/lib/systemd/user`, and writes env examples under
`~/.local/etc/ozma`. It never removes Python and never starts a unit. Then:

```bash
systemctl --user daemon-reload
systemctl --user link ~/.local/lib/systemd/user/ozma-node@vm.service
# fill in ~/.local/etc/ozma/ozma-node-vm.env first
systemctl --user enable --now ozma-node@vm.service
```

## System install (operator, requires root)

The installer itself never uses sudo. A system-wide deployment runs the same
layout with root and then:

```bash
sudo useradd --system --home /var/lib/ozma-node --shell /usr/sbin/nologin ozma-node
sudo install -d -m 0750 -o ozma-node -g ozma-node \
    /var/lib/ozma-node /run/ozma-node /run/ozma/qmp
sudo scripts/install-rust.sh --from-target target/release --system --prefix /usr/local
sudo cp /usr/local/share/ozma/env/*.env.example /etc/ozma/
# edit /etc/ozma/*.env, then:
sudo systemctl daemon-reload
sudo systemctl enable --now ozma-node@hardware.service
# or, on a hypervisor:
sudo systemctl enable --now ozma-virtual-node.service
```

`install-rust.sh` does not create the system user or chown directories; that is
an administrative decision and is left explicit.

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
- Keep `ProtectSystem=strict`, `NoNewPrivileges=yes`, and the restricted address
  families. Add only the `DeviceAllow=` entries the selected profile needs.
- Put `OZMA_API_TOKEN`/`OZMA_NODE_API_TOKEN` in the env file with mode `0600`;
  they are never written to the unit or logs.

## Validate units

```bash
systemd-analyze verify scripts/systemd/ozma-node@.service \
    scripts/systemd/ozma-node-vm@.service \
    scripts/systemd/ozma-virtual-node.service
```

`systemd-analyze` is not required on the build host; absence is reported as a
SKIP by `scripts/clean-host/check-deps.sh`.

## Host-level udev/AppArmor/libvirt configuration

The Python `softnode/install/install.sh` still owns the privileged udev rule,
AppArmor drop-in, and libvirt `cgroup_device_acl` setup. It works for the Rust
VM profile because both use the same `/dev/input/event*` and uinput paths. Port
that script into a Rust-owned installer before deleting `softnode/`; this
directory does not silently duplicate or replace it.
