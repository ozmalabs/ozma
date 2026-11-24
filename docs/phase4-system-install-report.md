# Phase 4 system install verification report

Date: 2026-11-25

Scope: validate the fresh-host system install surface in
`scripts/install-rust.sh`, `scripts/install-system.sh`,
`scripts/verify-system-install.sh`, `scripts/systemd/verify-units.sh`,
`scripts/clean-host/*`, and `scripts/system/` **without sudo and without
changing host state**. Temp roots and a rootless container were used for every
install. Nothing was committed and no Python tree was deleted.

Environment:

```text
host:      Linux 7.3.5 x86_64, uid 1000 (not root)
systemd:   262
udevadm:   262
podman:    6.2.0 (rootless, crun)
apparmor:  apparmor_parser not installed (SKIP)
```

## Verification matrix

| # | Check | Command | Result |
|---|---|---|---|
| 1 | Host install verification | `TMPDIR=/tmp/opencode scripts/verify-system-install.sh` | **PASS** — 77 passed, 0 failed, 1 skipped (`RESULT: PASS`) |
| 2 | Unit verification (system + user) | `scripts/systemd/verify-units.sh` | **PASS** — 3 system + 3 user units |
| 3 | Staged dry-run writes nothing | `scripts/install-system.sh --root /tmp/opencode/stage-dry2 --profile all --no-user --dry-run` | **PASS** — exit 0; root path absent |
| 4 | Real staged install | `scripts/install-system.sh --root /tmp/opencode/stage-all-nouser --profile all --no-user --usb-gadget` | **PASS** — exit 0; rendered tree below |
| 5 | Staged idempotence | repeat command from #4, compare SHA-256 of every file | **PASS** — no file changed |
| 6 | `udevadm verify` on rendered rules | `udevadm verify -N never <rule>` for all six rules | **PASS** — 6/6 |
| 7 | `systemd-analyze verify` on rendered units | staged copy with harmless stand-in binaries for `ExecStart` | **PASS** — exit 0 |
| 8 | `systemd-tmpfiles` syntax/dry-run | `systemd-tmpfiles --create --dry-run --graceful /tmp/opencode/stage-all-nouser/etc/tmpfiles.d/ozma-node.conf` | **PASS** — would create the three `/run/ozma*` dirs |
| 9 | AppArmor syntax | `apparmor_parser -Q <wrapper>` | **SKIP** — `apparmor_parser` not installed |
| 10 | `install-rust.sh` user layout | `scripts/install-rust.sh --from-target /tmp/opencode/fakebin --prefix /tmp/opencode/rinstall` | **PASS** — user units, env, binaries; no system-only directives; no placeholders |
| 11 | `install-rust.sh --system` dry-run | `scripts/install-rust.sh --system --dry-run --from-target /tmp/opencode/fakebin --prefix /tmp/opencode/rsys` | **PASS** — exit 0; prefix absent |
| 12 | Rootless container verification | `podman run --rm --userns=keep-id ... localhost/ozma-clean-host:1.95 /src/scripts/verify-system-install.sh` | **PASS** — 75 passed, 0 failed, 3 skipped (`udevadm`, `apparmor_parser`, `systemd-analyze` absent) |
| 13 | Full clean-host container harness | `scripts/clean-host/run.sh --container --no-smoke` | **FAIL (pre-existing product build)** — `ozma-usb/src/functionfs.rs` cannot resolve `crate::functionfs_daemon`; harness stops before its build/test stage. Not caused by this install work. |

Host run summary:

```text
== Summary ==
77 passed, 0 failed, 1 skipped
RESULT: PASS
```

The single host SKIP is AppArmor syntax (`apparmor_parser` absent). The
container run adds two more SKIPs because `udevadm` and `systemd-analyze` are
not in the build image.

## Staged install evidence

Dry-run:

```bash
scripts/install-system.sh \
  --root /tmp/opencode/stage-dry2 --profile all --no-user --dry-run
# exit 0
test -e /tmp/opencode/stage-dry2 || echo "no root created"
# no root created
```

Real staged install (`--no-user` means account commands are not run and are not
recorded):

```bash
scripts/install-system.sh \
  --root /tmp/opencode/stage-all-nouser \
  --profile all --no-user --usb-gadget
# exit 0
```

Owner/mode tree (staged owner is the invoking user because no host account is
created; a live root install substitutes `root`/`ozma-node`):

```text
755 matt:matt d .
755 matt:matt d ./etc
755 matt:matt d ./etc/apparmor.d
755 matt:matt d ./etc/apparmor.d/abstractions
755 matt:matt d ./etc/apparmor.d/abstractions/libvirt-qemu.d
644 matt:matt f ./etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
755 matt:matt d ./etc/modules-load.d
644 matt:matt f ./etc/modules-load.d/ozma-node.conf
750 matt:matt d ./etc/ozma
640 matt:matt f ./etc/ozma/install-operator-commands.txt
640 matt:matt f ./etc/ozma/libvirt-qemu-cgroup_device_acl.conf
640 matt:matt f ./etc/ozma/ozma-node-desktop.env.example
640 matt:matt f ./etc/ozma/ozma-node-hardware.env.example
640 matt:matt f ./etc/ozma/ozma-node-vm.env.example
640 matt:matt f ./etc/ozma/ozma-virtual-node.env.example
755 matt:matt d ./etc/tmpfiles.d
644 matt:matt f ./etc/tmpfiles.d/ozma-node.conf
755 matt:matt d ./etc/udev
755 matt:matt d ./etc/udev/rules.d
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-gadget.rules
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-hidg.rules
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-input.rules
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-render.rules
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-uinput.rules
644 matt:matt f ./etc/udev/rules.d/70-ozma-node-v4l2.rules
755 matt:matt d ./run
755 matt:matt d ./run/ozma
750 matt:matt d ./run/ozma-node
770 matt:matt d ./run/ozma/qmp
755 matt:matt d ./usr
755 matt:matt d ./usr/local
755 matt:matt d ./usr/local/lib
755 matt:matt d ./usr/local/lib/systemd
755 matt:matt d ./usr/local/lib/systemd/system
644 matt:matt f ./usr/local/lib/systemd/system/ozma-node@.service
755 matt:matt d ./usr/local/lib/systemd/system/ozma-node@.service.d
644 matt:matt f ./usr/local/lib/systemd/system/ozma-node@.service.d/10-usb-gadget.conf
644 matt:matt f ./usr/local/lib/systemd/system/ozma-node-vm@.service
644 matt:matt f ./usr/local/lib/systemd/system/ozma-virtual-node.service
755 matt:matt d ./var
755 matt:matt d ./var/lib
750 matt:matt d ./var/lib/ozma
750 matt:matt d ./var/lib/ozma-node
750 matt:matt d ./var/lib/ozma-node/hls
```

Rendered tmpfiles:

```text
d /run/ozma-node 0750 ozma-node ozma-node -
d /run/ozma      0755 ozma-node ozma-node -
d /run/ozma/qmp  0770 root        kvm  -
```

`/run/ozma` is owned by the service account (so the supervisor can write
`audio-provision.json`/markers) but is group/other-traversable so QEMU/libvirt
can reach `/run/ozma/qmp`.

Rendered modules-load (`--usb-gadget`):

```text
uinput
libcomposite
```

Unit account lines after rendering:

```text
User=ozma-node
Group=ozma-node
```

With `--user ozma-test --group ozma-test-grp`, all three units render
`User=ozma-test`/`Group=ozma-test-grp`, and tmpfiles/udev/operator commands use
the same names.

Profile-specific udev selection (verified):

| Profile | udev rules |
|---|---|
| `hardware` | hidg, render, v4l2 |
| `vm` | uinput, input, render |
| `desktop` | uinput, input, render |
| `all` | hidg, input, render, uinput, v4l2 |
| `all --usb-gadget` | above plus gadget |

Profile groups in the operator file (existing host groups only):

```text
hardware: video,input,render
vm:       input,render,kvm,libvirt
desktop:  input,video,render
all:      video,input,render,kvm,libvirt
```

`--no-user` omits all `groupadd`/`useradd`/`usermod` lines. A repeat identical
install produced byte-identical files.

## udev, AppArmor, units, tmpfiles

`udevadm verify -N never` (udev 262):

```text
PASS 70-ozma-node-gadget.rules
PASS 70-ozma-node-hidg.rules
PASS 70-ozma-node-input.rules
PASS 70-ozma-node-render.rules
PASS 70-ozma-node-uinput.rules
PASS 70-ozma-node-v4l2.rules
```

`systemd-analyze verify` on the staged units passes when harmless stand-ins are
placed at the staged `ExecStart` paths. Without them it reports only the
expected missing-binary error for a not-yet-installed node:

```text
Command .../usr/local/bin/ozma-node is not executable: No such file or directory
```

`scripts/systemd/verify-units.sh` (with its own stand-ins) passes all six:

```text
PASS  system ozma-node@.service
PASS  system ozma-node-vm@.service
PASS  system ozma-virtual-node.service
PASS  user ozma-node@.service
PASS  user ozma-node-vm@.service
PASS  user ozma-virtual-node.service
```

AppArmor: **SKIP**. `apparmor_parser` is absent; the drop-in content is
`/dev/input/event* rw,` under
`/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma`.

## Safety checks

- No `sudo` invocation in `install-system.sh`, `install-rust.sh`, or
  `verify-units.sh` (static check).
- Dry-run and staged installs created no host paths. After all runs,
  `/etc/ozma`, `/etc/udev/rules.d/70-ozma-node-*`, `/var/lib/ozma-node`, and
  `/run/ozma-node` were still absent on the host.
- No uncommented token/secret, private key, or world-writable path in the
  rendered tree.
- Env examples are `0640`, units/udev/tmpfiles/AppArmor are `0644`, the
  operator file and libvirt snippet are `0640`, `/etc/ozma` is `0750`.
- Existing libvirt `qemu.conf` is copied to a test root with a sentinel and is
  byte-identical after the installer runs; only
  `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf` is written. The installer
  prints the merge/restart instructions and never edits `qemu.conf`.
- The desktop caveat is present in the operator file and the desktop env
  example: prefer a `systemd --user` unit because a system unit cannot see a
  logged-in user's PipeWire session.
- `--no-env --no-dirs` still writes `install-operator-commands.txt`; it does not
  silently create the skipped units/env examples.
- Invalid `--user`/`--group` values containing whitespace, `&`, `|`, `@`, `/`,
  or `:` are rejected with exit 2.

## Bugs found and fixed

1. **`/run/ozma` ownership/mode disagreed between the installer and tmpfiles,
   and the tmpfiles mode blocked the service.** The installer created
   `/run/ozma` as `0750 ozma-node:ozma-node`; tmpfiles declared
   `0755 root:root`. At boot, tmpfiles would make the directory unwritable by
   the supervisor (`audio-provision.json`, markers), while `0750` would prevent
   QEMU/libvirt from traversing to `/run/ozma/qmp`. Fixed to
   `0755 ozma-node:ozma-node` in both `scripts/install-system.sh` and
   `scripts/system/tmpfiles/ozma-node.conf.in`, with a verification check.
2. **`--no-env --no-dirs` exited 1.** The operator file was installed into
   `$ENV_DIR` without ensuring the parent existed. `install-system.sh` now
   creates the operator-file parent with the normal `0750 root:ozma-node`
   ownership; a toggle-composition test was added.
3. **`--user`/`--group` were not honored by the systemd units.** Units
   hard-coded `User=ozma-node`/`Group=ozma-node` while dirs, tmpfiles, udev, and
   the operator file used the custom names. The units now use
   `@OZMA_USER@`/`@OZMA_GROUP@`; `install-system.sh`, `install-rust.sh`, and
   `verify-units.sh` render them; account names are validated. A verification
   check covers custom account rendering across all assets.
4. **Verification gaps.** `verify-system-install.sh` now also checks profile
   groups, udev group contents, runtime-dir modes, tmpfiles/installer agreement,
   copy-only `qemu.conf` behavior, the desktop user-unit caveat, toggle
   composition, and the absence of `sudo` invocations. That raised the host
   count from 54 to 77.

No product Rust code was changed.

One ordering note: `install-rust.sh --system` creates state/runtime directories
with plain `mkdir` as a convenience. The documented fresh-host sequence runs
`install-system.sh` afterward, which sets the final service ownership and modes
(including `/run/ozma-node` `0750` and `/run/ozma` `0755`); running
`install-system.sh` first is also safe because its `install -d` is idempotent.

## Remaining root-only steps

1. Run the live install as root on a fresh host, for example
   `sudo scripts/install-system.sh --profile hardware` (and the desired profile
   set). This creates the `ozma-node` account/groups, chowns state/runtime
   dirs, writes `/etc/ozma`, `/etc/udev`, `/etc/apparmor.d`, `/etc/tmpfiles.d`,
   and `/etc/modules-load.d`, and installs units.
2. Apply host state exactly as printed in
   `/etc/ozma/install-operator-commands.txt`:
   `systemctl daemon-reload`, `udevadm control --reload-rules`,
   `udevadm trigger --action=change`,
   `systemd-tmpfiles --create /etc/tmpfiles.d/ozma-node.conf`, and
   `systemctl reload apparmor` only when AppArmor is active.
3. Merge `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf` into
   `/etc/libvirt/qemu.conf` (extend an existing `cgroup_device_acl` rather than
   duplicating the key), then `systemctl restart libvirtd` and restart VMs.
4. Enable only the selected profile:
   `systemctl enable --now ozma-node@hardware.service`,
   `ozma-node@vm.service`, `ozma-virtual-node.service`, or a per-VM
   `ozma-node-vm@<name>.service`.
5. For USB gadget: mount ConfigFS, provide a real UDC, and arrange writable
   gadget ownership; the node does not receive `CAP_SYS_ADMIN`.
6. For desktop: prefer `scripts/install-rust.sh --build --prefix "$HOME/.local"`
   plus `systemctl --user enable --now ozma-node@desktop.service`; a system
   `ozma-node@desktop.service` needs explicit session access and relaxed
   `ProtectHome`/`ReadWritePaths`.
7. Re-run `scripts/clean-host/run.sh --container` once the pre-existing
   `ozma-usb::functionfs_daemon` compile error is resolved. This report's
   container evidence covers the install/verify surface, not the product build
   or tests.

## Cleanup

All temp roots, stand-in binaries, copies, and the temporary container image
used for this validation were removed after the run. No host account, udev
rule, AppArmor profile, libvirt configuration, unit, or service was created,
enabled, started, reloaded, or edited.
