# Phase 4 release-profile install report

Date: 2026-11-25 20:11–20:19 (Australia/Sydney)
Host: `SFFPC`, Linux 7.3.5-2-cachyos, systemd 262 (262.2-1-arch), x86_64
Tree: `/home/matt/ozma/ozma` at `8b951af` **plus uncommitted work**. No commit,
tag, or worktree was created.
Status: **release build PASS; release install PASS; verifier PASS
(82 passed, 0 failed, 1 skipped); no unit enabled or started.**

This report closes the debug-profile deviation recorded in
[`phase4-privileged-install-report.md`](phase4-privileged-install-report.md)
§7.3. It does **not** start a node/desktop service and does not exercise live
capture, input, audio, PipeWire, HID, QMP, or USB-gadget data paths.

## 1. Release build

Command (bounded to 4 jobs to avoid OOM; 12 cores, 30 GiB RAM):

```bash
cargo build --release -j 4 -p ozma-node -p ozma-softnode -p ozma-multiseat
```

Result: exit 0, `Finished release profile [optimized] target(s) in 1m 44s`
(105 s wall). Warnings only (`ozma-pipewire` unused imports/dead code;
`ozma-softnode` unused `VMInfo` fields). No test suite was run.

`ozma-softnode` builds both of its bins, `ozma-virtual-node` and
`ozma-meeting-detect`, so the command above covers the meeting detector as
well.

| Binary | Bytes | SHA-256 |
|---|---:|---|
| `target/release/ozma-node` | 37,527,256 | `e87a3743ee42b7e6a4201661c27ce42d5093a8101bd5721de7202b636e557ef9` |
| `target/release/ozma-virtual-node` | 10,010,344 | `9c67fe78efeef2bf658d21837c772fa74a186041d62599bc0c4ef89da0456d37` |
| `target/release/ozma-multiseat` | 4,206,808 | `0cf258360662d4e09fb7453b02a6e741b115380368f9d34aeacdeed81195df17` |
| `target/release/ozma-meeting-detect` | 2,917,248 | `c5efb0beb0220cb5854223c3d878ff76095aa826c6e5fb2b90a60aa1e5f3032f` |

All four are stripped of debug info compared with the debug binaries
(`file`: no `with debug_info`); they are dynamically linked ELF PIE binaries,
not stripped of symbols.

CLI smoke output from `target/release` before install:

| Command | Output |
|---|---|
| `ozma-node --version` | `ozma-node 0.1.0` |
| `ozma-node --help` | `Ozma hardware KVM node — mDNS, controller registration, session API, UAC2 bridge` |
| `ozma-virtual-node --help` | `Discover running VMs on this hypervisor, report their status, provision the in-guest agent, and supervise one \`ozma-node --profile vm\` subprocess per discovered VM that has a QMP socket.` |
| `ozma-multiseat --help` | `Enumerate X11 outputs and read-only input topology, plan one seat per display, print or validate the plan, and optionally spawn and supervise one \`ozma-node --profile desktop\` child per seat.` |
| `ozma-meeting-detect --help` | no help mode; unknown args are ignored and the detector starts (see §6.1) |

## 2. Installed build

The earlier gate left **debug** binaries of a different size/hash. Therefore
`install-rust.sh` required `--force` to replace them.

```bash
sha256sum /usr/local/bin/ozma-*                  # pre-install: debug hashes
sudo -n scripts/install-rust.sh --system --prefix /usr/local \
  --from-target target/release --force           # rc 0
scripts/install-rust.sh --prefix "$HOME/.local" \
  --from-target target/release --force           # rc 0
sudo -n scripts/install-system.sh --profile all --usb-gadget  # rc 0, idempotent
```

`install-system.sh` was re-run only for the idempotence check. It kept all five
existing env examples, rewrote the four units (byte-identical hashes to the
earlier gate), and installed/changed no new host asset.

Installed binary hashes match `target/release` exactly in both layouts:

| Path | Bytes | SHA-256 |
|---|---:|---|
| `/usr/local/bin/ozma-node` | 37,527,256 | `e87a3743…6e557ef9` |
| `/usr/local/bin/ozma-virtual-node` | 10,010,344 | `9c67fe78…a0456d37` |
| `/usr/local/bin/ozma-multiseat` | 4,206,808 | `0cf25836…1195df17` |
| `/usr/local/bin/ozma-meeting-detect` | 2,917,248 | `c5efb0be…e5f3032f` |
| `~/.local/bin/ozma-{node,virtual-node,multiseat,meeting-detect}` | same | same |

System binaries are `0755 root:root`, mtime `2026-11-25 20:16:25`; user
binaries are `0755 matt:matt`, mtime `2026-11-25 20:16:29`. The WebRTC helper is
unchanged (`fc799016…55eacd`) in both layouts.

## 3. Verification evidence

All checks below were run as the normal user unless marked `sudo`.

| Check | Result | Evidence |
|---|---|---|
| `scripts/systemd/verify-units.sh` | **PASS 8/8** | `PASS system` + `PASS user` for all four units |
| `systemd-analyze verify` installed system units | **PASS** | four unit files, `rc=0` |
| `systemd-analyze verify ozma-node@hardware.service` | **PASS** | `rc=0`; applies the USB-gadget drop-in |
| `systemd-analyze --user verify` installed user units | **PASS** | four files, `rc=0` |
| `scripts/verify-system-install.sh` | **PASS** | `82 passed, 0 failed, 1 skipped`; skip is AppArmor (`apparmor_parser` absent) |
| Transient unit: `ozma-node --version` | **PASS** | `ozma-node 0.1.0`; `status=0/SUCCESS`, 12 ms, 4 M peak |
| Transient unit: `ozma-node --help` | **PASS** | `status=0/SUCCESS` |
| Transient unit: `ozma-virtual-node --help` | **PASS** | `status=0/SUCCESS` |
| Transient unit: `ozma-multiseat --help` | **PASS** | `status=0/SUCCESS` |
| `ozma-meeting-detect --help` | **N/A (no help mode)** | unknown args are ignored; it starts the detector. Not run under `systemd-run` |
| Enabled/active units | **PASS** | four unit files `disabled`; `systemctl list-units 'ozma*' --all` → 0 |
| `qemu.conf` untouched | **PASS** | SHA-256 still `2c7a8e5f…3e55` |

Transient units were all `--collect`ed and no longer exist:

```bash
sudo -n systemd-run --wait --pipe --collect --unit=ozma-rel-node-version \
  --property=User=ozma-node --property=Group=ozma-node \
  --property=NoNewPrivileges=yes /usr/local/bin/ozma-node --version
# ... same shape for ozma-node --help, ozma-virtual-node --help,
#     and ozma-multiseat --help
# each: Finished with result: success, status=0/SUCCESS
```

After install, `sudo -n systemctl daemon-reload` returned 0 so the manager's
unit cache matches the rewritten files. No unit was enabled, started, or
loaded; `systemctl is-active ozma-node@hardware.service` → `inactive` (rc 3).

## 4. Optional hardware smoke: skipped by design

`ozma-node --profile hardware --capture-provider fake` cannot be used: the
configuration rejects the synthetic provider for the hardware profile so it
cannot mask a missing V4L2 source or become a production default.

```bash
./target/release/ozma-node --profile hardware --capture-provider fake \
  --fake-width 64 --fake-height 64 --fake-fps 1
# ERROR Invalid node configuration: --capture-provider fake is a dev/test
# synthetic source and is not available in the hardware profile;
# use --profile vm or desktop
# rc=2 (parse-time exit; no service, capture, or device access)
```

Starting a `vm`/`desktop` node merely to use the fake source would open
listeners and advertise/register a node, which is outside this install gate.
The hardware-profile smoke is therefore skipped; it needs a real V4L2/HID host
or a dedicated `--profile vm --capture-provider fake` runtime gate.

## 5. Mismatch found and fixed

The earlier install left the five `/etc/ozma/*.env.example` files
`0640 root:root`, while the operator file and libvirt snippet are
`0640 root:ozma-node` and `install-system.sh` intends `root:$SYS_GROUP`.
This is the §7.2 mismatch of the previous report.

```bash
sudo -n chgrp ozma-node /etc/ozma/ozma-{multiseat,node-desktop,node-hardware,node-vm,virtual-node}.env.example
```

Result: all five are now `0640 root:ozma-node`; content and mode are unchanged.
No other ownership/mode mismatch was found (`/etc/ozma` `0750
root:ozma-node`; `/var/lib/ozma-node`, `/var/lib/ozma`, `/run/ozma*` as
documented; user env examples `0640 matt:matt`).

The installer-side defect remains: `install-rust.sh --system` creates missing
env examples with `install -m 0640` (root:root), and `install-system.sh`
deliberately keeps existing files. A fresh install in that order will recreate
the mismatch unless `install-rust.sh` uses `-g ozma-node` or `install-system.sh`
`chgrp`s kept examples.

## 6. Deviations and findings

### 6.1 `ozma-meeting-detect` has no help/version mode

`parse_args` ignores unknown arguments, so `--help` starts the detector. A
direct `./target/release/ozma-meeting-detect --help` was attempted during CLI
discovery; it ran until the 120 s tool timeout was reached and was killed. It
created `/run/user/1000/ozma-meeting-detect.sock` and made no other host change;
the socket was removed afterwards. It was **not** run under `systemd-run` and no
meeting server was intentionally started. Its installed binary is covered by
hash comparison, but not by a help smoke test.

### 6.2 Dirty tree

The binaries are built from `8b951af` plus the uncommitted AIDA64/display
workstream, not from a frozen commit. The artifact hashes above identify this
build only.

### 6.3 No daemon-reload was required for verification

`systemd-analyze verify` reads the unit files directly. A single
`systemctl daemon-reload` was still run after install to make the manager's
cache match the installed files; it neither enabled nor started anything.

## 7. Leftover state

- Four disabled system units, four disabled user units (user units not loaded;
  `systemctl --user list-units 'ozma*' --all` → 0).
- Release binaries in `/usr/local/bin` and `~/.local/bin`.
- Host integration from the earlier gate remains: 6 udev rules (still not
  reloaded/triggered), AppArmor drop-in (parser absent), tmpfiles/modules-load
  snippets (not applied/loaded), copy-only libvirt snippet, `/etc/ozma`, 5 env
  examples, and the `ozma-node` account/group.
- `/etc/libvirt/qemu.conf` is byte-identical to the pre-gate hash.
- No node/desktop/multiseat/meeting service started; no GPU/audio/USB/input
  interaction.
- Build/test logs were kept only in `/tmp/opencode/ozma-release-gate` during
  the gate and removed after this report was written.
- No commit, tag, container image, venv, or new worktree was created.

## 8. Remaining privileged gaps

1. No node service was started; live capture/input/audio/WebRTC/HID/QMP paths
   are still unexercised.
2. udev rules are syntax-verified but not applied to the running system.
3. AppArmor drop-in is unparsed (`apparmor_parser`/service absent).
4. The libvirt `cgroup_device_acl` snippet is not merged into `qemu.conf`.
5. USB-gadget ConfigFS/UDC setup was not exercised.
6. `ozma-meeting-detect` has no `--help`/`--version` and no unit; it was not
   smoke-started deliberately.
7. Evidence is for a dirty working tree (`8b951af` + uncommitted work), with no
   frozen release tag or reproducible-build check.
8. No release test suite (`cargo test --release` or workspace tests) was run;
   only the build, install, unit syntax, and transient `--help` smokes.
