# Phase 4 privileged fresh-host install report

Date: 2026-11-25 18:29–18:34 (Australia/Sydney)
Host: `SFFPC`, Linux 7.3.5-2-cachyos, systemd 262 (262.2-1-arch), x86_64
Tree: `/home/matt/ozma/ozma` at `5ae55e7` **plus uncommitted work** (the AIDA64 /
display importers workstream). No commit or tag was created.
Status: **install gate PASS; one verifier check FAIL (root-caused to live-state
coupling, not an install defect); one SKIP (no `apparmor_parser`).** The install
was left in place. Rollback is in §8.

This is the real privileged install the audit marked unrun
([`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16, P0 blocker 3). It
closes the "install surface is only validated by dry-run / `--root`" gap for
this host; it does not close the other P0s (live demo, container gate already
separately green, lockfile, rollback tag).

## 1. Pre-install state

Snapshot commands and files are reproducible from the list below. All were
taken before any write.

```bash
ls -la /etc/ozma
getent passwd ozma-node; getent group ozma-node
getent group video input render kvm libvirt
ls -la /usr/local/lib/systemd/system/ozma*
systemctl list-unit-files 'ozma*'
ls -la /etc/systemd/system/ozma* /etc/systemd/system/*.wants/ozma*
ls -la /etc/udev/rules.d/70-ozma*
ls -la /etc/apparmor.d/abstractions/libvirt-qemu.d
ls -la /etc/tmpfiles.d/ozma* /etc/modules-load.d/ozma*
ls -la /var/lib/ozma* /run/ozma*
ls -la /usr/local/bin/ozma*
ls -la ~/.local/bin/ozma* ~/.local/etc/ozma ~/.local/var ~/.local/lib/systemd/user
systemctl --user list-unit-files 'ozma*'
sha256sum /etc/libvirt/qemu.conf
```

Results (nothing Ozma-related was present):

| Item | Pre-state |
|---|---|
| `/etc/ozma` | absent |
| `ozma-node` user/group | absent |
| `video`, `input`, `render`, `kvm`, `libvirt` groups | present (985, 994, 989, 992, 956) |
| `/usr/local/lib/systemd/system/ozma*` | absent |
| `systemctl list-unit-files 'ozma*'` | 0 units |
| `/etc/udev/rules.d/70-ozma*` | absent |
| `/etc/apparmor.d/abstractions/libvirt-qemu.d` | absent (parent did not exist) |
| `/etc/tmpfiles.d/ozma-node.conf`, `/etc/modules-load.d/ozma-node.conf` | absent |
| `/var/lib/ozma*`, `/run/ozma*` | absent |
| `/usr/local/bin/ozma*` | absent |
| `~/.local/{bin,etc,var,lib/systemd/user}` Ozma paths | absent (only unrelated `ozma`, `ozma-mobile-sync`, `ozma-stats` scripts existed) |
| `/etc/libvirt/qemu.conf` | present, `sha256 2c7a8e5f199f87c6ea6896e638c8df34aca88e07bff7a54855db6e20678e3e55`; `cgroup_device_acl` commented out |
| user units | 0 units |

Backups taken: `/etc/libvirt/qemu.conf` copied byte-for-byte; its SHA-256 was
recorded. Because the pre-state is empty, the restore path is deletion, not
restoration.

## 2. Binaries used

The checkout had only a stale `target/release/ozma-node`. To run a real install
without a multi-GB release rebuild on a host with ~6.8 GB free, the gate used
the **debug-profile** binaries from the current tree instead. This is a
deviation from a production release install and is recorded in §7.

```bash
cargo build --offline -p ozma-softnode      # 3.9 s; refreshed virtual-node + meeting-detect
```

Installed binaries (both layouts):

| Binary | Bytes | SHA-256 |
|---|---:|---|
| `ozma-node` | 404,484,456 | `5918fda095e511b08cb9eefa0cd629f881d377de86c51a79e7736201e4277469` |
| `ozma-virtual-node` | 136,912,840 | `08c7ac2e4a15d21818fe0b9642d37bcfbab6f9f29c04ab297d2aef26981d8130` |
| `ozma-multiseat` | 81,410,472 | `87199207b1b705a35b7122471476e0365d671a68961832d3fc576d355719a8d0` |
| `ozma-meeting-detect` | 46,563,824 | `9155e64ae9b9d9680749fd42fb76c483ec91843e96d06733bcb07872c3ee95b2` |
| WebRTC helper | 36,306 | `fc7990162651f18dca5d91c3ea3dbe6a2351e09d30f2a976add51febfd55eacd` |

`ozma-node --version` prints `ozma-node 0.1.0`; the other two declare no clap
version (their `--version` exits with "unexpected argument"), matching the
known CLI surface.

## 3. Exact install commands

```bash
# dry run first (non-root; writes nothing)
./scripts/install-system.sh --dry-run --profile all --usb-gadget
# exit 0; listed exactly the assets installed below

# 1. non-root user layout for the invoking user
./scripts/install-rust.sh --from-target target/debug --prefix "$HOME/.local"
# exit 0; no unit enabled/started

# 2. system binaries/units/env examples so the system units resolve ExecStart
sudo -n scripts/install-rust.sh --from-target target/debug --system --prefix /usr/local
# exit 0; no unit enabled/started

# 3. privileged host integration (no --reload deliberately)
sudo -n scripts/install-system.sh --profile all --usb-gadget
# exit 0

# 4. idempotence check
sudo -n scripts/install-system.sh --profile all --usb-gadget
# exit 0; all env examples "kept existing"; operator file rewritten identically
```

`--usb-gadget` is supported and was used. No unit was enabled or started. The
installer never edits `/etc/libvirt/qemu.conf`; the snippet is copy-only.

## 4. Verification evidence

| Check | Result | Evidence |
|---|---|---|
| `scripts/verify-system-install.sh` (real assets) | **FAIL 81/1/1** | `RESULT: FAIL`, `81 passed, 1 failed, 1 skipped`; failure is one live-root dry-run check, root-caused in §7.1 |
| `scripts/systemd/verify-units.sh` | **PASS** | 8/8: `PASS system` and `PASS user` for all four units |
| `systemd-analyze verify` installed units | **PASS** | `ozma-node@.service`, `ozma-node-vm@.service`, `ozma-virtual-node.service`, `ozma-multiseat.service` all `rc=0`; `ozma-node@hardware.service` (drop-in applied) `rc=0` |
| `udevadm verify /etc/udev/rules.d/70-ozma-node-*.rules` | **PASS** | `6 udev rules files have been checked. Success: 6  Fail: 0` |
| `udevadm test /sys/class/mem/null` (safe, non-Ozma device) | **PASS** | `rc=0`; parses rules without touching an Ozma/desktop device |
| `systemd-tmpfiles --create --dry-run /etc/tmpfiles.d/ozma-node.conf` | **PASS** | `rc=0`; "Would create directory" `/run/ozma-node`, `/run/ozma`, `/run/ozma/qmp` (already present from the installer) |
| AppArmor parser check | **SKIP** | `apparmor_parser` not installed; `apparmor.service` `inactive`/`not-found`; drop-in installed but not parsed |
| Account/group rendering | **PASS** | `ozma-node:x:942:942::/var/lib/ozma-node:/usr/sbin/nologin`; `id`: `groups=942,994(input),992(kvm),989(render),985(video),956(libvirt)` |
| Unit account rendering | **PASS** | `ozma-node@`: `User/Group=ozma-node`, `SupplementaryGroups=video input render`; `ozma-node-vm@`: `kvm input render`; `ozma-virtual-node`: `libvirt kvm input video render`; `ozma-multiseat`: `video input render` |
| `/run/ozma` accessibility | **PASS** | `/run/ozma` `0755 ozma-node:ozma-node`; `/run/ozma-node` `0750`; `/run/ozma/qmp` `0770 root:kvm`; `ozma-node` can write `/run/ozma` and traverse `/run/ozma/qmp` |
| Seats file path | **PASS** | unit: `--seats-file /var/lib/ozma-node/seats.json`; file correctly absent before first use |
| Multiseat unit | **PASS** | `ExecStart=/usr/local/bin/ozma-multiseat --node-bin /usr/local/bin/ozma-node --seats-file /var/lib/ozma-node/seats.json --hotplug $OZMA_MULTISEAT_ARGS`; no `--print-plan`/`--dry-run`; `Restart=on-failure` |
| Operator commands file | **PASS** | `/etc/ozma/install-operator-commands.txt` `0640 root:ozma-node`, 2,096 bytes, lists apply/enable/rollback commands; profiles `hardware vm desktop` |
| No unit enabled/active | **PASS** | `systemctl list-units 'ozma*' --all` → 0 loaded; `list-unit-files` → all `disabled` |
| `qemu.conf` untouched | **PASS** | SHA-256 still `2c7a8e5f…3e55`; `cmp` identical to the pre-install copy |
| User layout units | **PASS** | `systemd-analyze --user verify` on all four `~/.local/lib/systemd/user/ozma-*.service` → `rc=0`; user units have no `User=`/`Group=` and `WantedBy=default.target` |

### 4.1 Safe transient unit exercise

A transient unit ran the installed binary as the installed account. No installed
unit was enabled or started, and nothing captured the desktop.

```bash
sudo -n systemd-run --wait --pipe --collect --unit=ozma-gate-node-help \
  --property=User=ozma-node --property=Group=ozma-node \
  --property=NoNewPrivileges=yes /usr/local/bin/ozma-node --help
# Finished with result: success
# Main processes terminated with: code=exited, status=0/SUCCESS
# Service runtime: 82ms; memory peak 57.1M

sudo -n systemd-run --wait --pipe --collect --unit=ozma-gate-id \
  --property=User=ozma-node --property=Group=ozma-node /usr/bin/id
# uid=942(ozma-node) gid=942(ozma-node)
# groups=942(ozma-node),956(libvirt),985(video),989(render),992(kvm),994(input)
```

Journal:

```text
Nov 25 18:31:36 SFFPC systemd[1]: Started [systemd-run] /usr/local/bin/ozma-node --help.
Nov 25 18:31:36 SFFPC systemd[1]: ozma-gate-node-help.service: Deactivated successfully.
```

Both transient units were `--collect`ed; `systemctl status ozma-gate-node-help`
reports "could not be found" (no leftover unit).

## 5. Asset inventory

Modes below are from `stat`/`find -printf` on the installed files.

### System

| Path | Mode | Owner | Bytes |
|---|---|---|---|
| `/usr/local/bin/ozma-node` | 0755 | root:root | 404,484,456 |
| `/usr/local/bin/ozma-virtual-node` | 0755 | root:root | 136,912,840 |
| `/usr/local/bin/ozma-multiseat` | 0755 | root:root | 81,410,472 |
| `/usr/local/bin/ozma-meeting-detect` | 0755 | root:root | 46,563,824 |
| `/usr/local/share/ozma/webrtc/gst_webrtc_worker.py` | 0644 | root:root | 36,306 |
| `/usr/local/lib/systemd/system/ozma-node@.service` | 0644 | root:root | 1,533 |
| `/usr/local/lib/systemd/system/ozma-node-vm@.service` | 0644 | root:root | 1,242 |
| `/usr/local/lib/systemd/system/ozma-virtual-node.service` | 0644 | root:root | 1,568 |
| `/usr/local/lib/systemd/system/ozma-multiseat.service` | 0644 | root:root | 2,053 |
| `/usr/local/lib/systemd/system/ozma-node@.service.d/10-usb-gadget.conf` | 0644 | root:root | 497 |
| `/etc/udev/rules.d/70-ozma-node-uinput.rules` | 0644 | root:root | 380 |
| `/etc/udev/rules.d/70-ozma-node-input.rules` | 0644 | root:root | 642 |
| `/etc/udev/rules.d/70-ozma-node-hidg.rules` | 0644 | root:root | 407 |
| `/etc/udev/rules.d/70-ozma-node-v4l2.rules` | 0644 | root:root | 284 |
| `/etc/udev/rules.d/70-ozma-node-render.rules` | 0644 | root:root | 295 |
| `/etc/udev/rules.d/70-ozma-node-gadget.rules` | 0644 | root:root | 640 |
| `/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma` | 0644 | root:root | 453 |
| `/etc/tmpfiles.d/ozma-node.conf` | 0644 | root:root | 646 |
| `/etc/modules-load.d/ozma-node.conf` | 0644 | root:root | 196 |
| `/etc/ozma/ozma-*.env.example` (5) | 0640 | root:root | 749–1,554 |
| `/etc/ozma/libvirt-qemu-cgroup_device_acl.conf` | 0640 | root:ozma-node | 1,389 |
| `/etc/ozma/install-operator-commands.txt` | 0640 | root:ozma-node | 2,096 |
| `/etc/ozma` | 0750 | root:ozma-node | — |
| `/var/lib/ozma-node`, `/var/lib/ozma` | 0750 | ozma-node:ozma-node | — |
| `/var/lib/ozma-node/hls` | 0750 | ozma-node:ozma-node | — |
| `/run/ozma-node` | 0750 | ozma-node:ozma-node | — |
| `/run/ozma` | 0755 | ozma-node:ozma-node | — |
| `/run/ozma/qmp` | 0770 | root:kvm | — |

Unit SHA-256 prefixes: `ozma-node@` `de5ff541`, `ozma-node-vm@` `3b1639e9`,
`ozma-virtual-node` `4ad0cf84`, `ozma-multiseat` `d3c894f0`.
udev, AppArmor, tmpfiles and modules-load checksums are in the run log
(`/tmp/opencode/ozma-privileged-gate/logs/inventory.txt`, deleted in §9).

### User (`matt`, `~/.local`)

| Path | Mode | Bytes |
|---|---|---|
| `~/.local/bin/ozma-{node,virtual-node,multiseat,meeting-detect}` | 0755 | same four binaries |
| `~/.local/share/ozma/webrtc/gst_webrtc_worker.py` | 0644 | 36,306 |
| `~/.local/lib/systemd/user/ozma-*.service` (4) | 0644 | 914–1,720 |
| `~/.local/etc/ozma/ozma-*.env.example` (5) | 0640 | 749–1,554 |
| `~/.local/var/lib/ozma-node`, `~/.local/var/lib/ozma`, `~/.local/var/run/ozma-node`, `~/.local/var/run/ozma` | directories | — |

## 6. Package / host state left behind

- One system account and group: `ozma-node` (uid 942, gid 942), supplementary
  groups `input kvm render video libvirt`.
- Four system units installed, **all disabled and inactive**; the USB-gadget
  drop-in is installed.
- Four user units installed under `~/.local`, **not enabled**; `systemctl --user
  daemon-reload` was not run, so the user manager has not loaded them.
- Six udev rules on disk. `udevadm control --reload-rules` and `udevadm trigger`
  were **not** run. The running system therefore still has
  `/dev/uinput` `root:input 0660` and `/dev/dri/renderD128` `root:render 0666`
  exactly as before. On the next boot/udev trigger the rules will apply;
  `/dev/uinput` will become `ozma-node:0660` by design.
- One AppArmor drop-in, not loaded (`apparmor` inactive, parser absent).
- `tmpfiles` and `modules-load` snippets installed; tmpfiles was applied only as
  `--dry-run`, modules were not loaded.
- `/etc/ozma` + 5 env examples + copy-only libvirt snippet + operator commands.
- No `/etc/libvirt/qemu.conf` change (byte-identical).
- No Python venv, no Python node package, no worktree, no container image.
- No unit enabled or started; no desktop/GPU state touched.

## 7. Deviations and findings

### 7.1 `verify-system-install.sh` fails on a live host installed with `--usb-gadget`

```text
PASS  ... (80 other checks)
FAIL  install-system.sh --dry-run without --root (non-root)
error: /etc/modules-load.d/ozma-node.conf exists and differs; re-run with --force to replace it
81 passed, 1 failed, 1 skipped
RESULT: FAIL
```

Cause: the verifier runs `install-system.sh --dry-run --profile hardware`
against the **live** root. The live `/etc/modules-load.d/ozma-node.conf` was
installed by this gate with `--usb-gadget`, so it contains `uinput` **and**
`libcomposite`; the hardware-only dry run renders `uinput` only, and
`install_prepared` refuses a differing existing file even in `--dry-run`.

This is a **verifier/live-state coupling**, not an install defect. Reproduction:

```bash
./scripts/install-system.sh --dry-run --profile all --usb-gadget  # rc=0 (matches live)
./scripts/install-system.sh --dry-run --profile hardware          # rc=1 (differs only by libcomposite)
```

Recommended fix (not applied here): make the live-root dry-run check render into
a temporary `--root`, or add `--force`/skip existence comparison in dry-run
mode. The other 80 checks and the independent unit checks are green.

### 7.2 Env examples are `root:root`, not `root:ozma-node`

Following the documented order (`install-rust.sh --system` first, then
`install-system.sh`) installs `/etc/ozma/ozma-*.env.example` as `0640 root:root`
because `install-rust.sh` uses `install -m 0640` without a group and
`install-system.sh` deliberately keeps existing env files. The expected layout
in `verify-system-install.sh` is `root:ozma-node`. No functional impact: systemd
reads `EnvironmentFile=` as root before dropping privileges, and the examples
contain no secrets. Recommended fix: `install-rust.sh --system` should use
`root:ozma-node`, or `install-system.sh` should `chgrp` kept env examples.

### 7.3 Debug-profile binaries

The gate installed `target/debug` binaries (sizes in §2), not release binaries.
The install/unit/account/udev surface is identical; runtime performance and
release-artifact reproducibility are not covered. A release install should be
rerun before packaging sign-off.

### 7.4 AppArmor skipped

`apparmor_parser` is absent and `apparmor.service` is inactive/not-found on this
host, so the drop-in was installed but not parsed. `verify-system-install.sh`
records this as `SKIP`, as designed.

### 7.5 Transient daemon-reload timeout

The first `systemctl daemon-reload` after `install-rust.sh --system` returned
`Reload daemon failed: Method call timed out`; an immediate retry returned
`rc=0` and the units are visible. Treat as a transient host/systemd load issue,
not an install failure.

### 7.6 Drop-in is not a standalone unit

`systemd-analyze verify .../ozma-node@.service.d/10-usb-gadget.conf` necessarily
fails (`Invalid argument`) because drop-ins are fragments. Verification used
`systemd-analyze verify ozma-node@hardware.service`, which applies the drop-in
and returns `rc=0`.

## 8. Decision and rollback

**Decision: LEAVE THE INSTALL IN PLACE.** It is complete, idempotent, matches
the rendered layout, has no enabled/started units, left the live device tree
untouched, and is fully reversible because the pre-state was empty.

Exact rollback:

```bash
# 1. Stop/disable only if they were later enabled (none are enabled now)
sudo -n systemctl disable --now ozma-node@hardware.service ozma-node@vm.service \
  ozma-node@desktop.service ozma-virtual-node.service ozma-multiseat.service 2>/dev/null || true

# 2. Remove system units and the gadget drop-in
sudo -n rm -f /usr/local/lib/systemd/system/ozma-node@.service \
  /usr/local/lib/systemd/system/ozma-node-vm@.service \
  /usr/local/lib/systemd/system/ozma-virtual-node.service \
  /usr/local/lib/systemd/system/ozma-multiseat.service
sudo -n rm -rf /usr/local/lib/systemd/system/ozma-node@.service.d

# 3. Remove host integration
sudo -n rm -f /etc/udev/rules.d/70-ozma-node-*.rules
sudo -n rm -f /etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
sudo -n rmdir /etc/apparmor.d/abstractions/libvirt-qemu.d 2>/dev/null || true
sudo -n rm -f /etc/tmpfiles.d/ozma-node.conf /etc/modules-load.d/ozma-node.conf

# 4. Remove system binaries/helper and all Ozma state/config created by this gate
sudo -n rm -f /usr/local/bin/ozma-node /usr/local/bin/ozma-virtual-node \
  /usr/local/bin/ozma-multiseat /usr/local/bin/ozma-meeting-detect
sudo -n rm -f /usr/local/share/ozma/webrtc/gst_webrtc_worker.py
sudo -n rmdir /usr/local/share/ozma/webrtc /usr/local/share/ozma 2>/dev/null || true
sudo -n rm -rf /etc/ozma /var/lib/ozma-node /var/lib/ozma /run/ozma-node /run/ozma

# 5. Remove the service account
sudo -n userdel ozma-node
sudo -n groupdel ozma-node

# 6. Refresh daemons
sudo -n systemctl daemon-reload

# 7. Remove the non-root user layout
rm -f ~/.local/bin/ozma-node ~/.local/bin/ozma-virtual-node \
  ~/.local/bin/ozma-multiseat ~/.local/bin/ozma-meeting-detect
rm -f ~/.local/lib/systemd/user/ozma-node@.service \
  ~/.local/lib/systemd/user/ozma-node-vm@.service \
  ~/.local/lib/systemd/user/ozma-virtual-node.service \
  ~/.local/lib/systemd/user/ozma-multiseat.service
rm -f ~/.local/share/ozma/webrtc/gst_webrtc_worker.py
rmdir ~/.local/share/ozma/webrtc ~/.local/share/ozma 2>/dev/null || true
rm -rf ~/.local/etc/ozma
rm -rf ~/.local/var/lib/ozma-node ~/.local/var/lib/ozma \
  ~/.local/var/run/ozma-node ~/.local/var/run/ozma
rmdir ~/.local/var/lib ~/.local/var/run ~/.local/var 2>/dev/null || true
systemctl --user daemon-reload    # if the user units were ever loaded
```

Do **not** touch `/etc/libvirt/qemu.conf` during rollback: it was never edited
(verified identical). If an operator later merges the copy-only snippet, that
merge must be reverted manually.

## 9. Cleanup

- The verifier's temporary roots were removed by their traps (verified: no
  `/tmp/ozma-system-install.*`, `ozma-system-verify.*`, or `ozma-unit-verify.*`).
- This gate's logs/snapshot directory (`/tmp/opencode/ozma-privileged-gate`) was
  removed after the report was written.
- No venv, container image, or git worktree was created; no commit was made.
- The only build artifact change is the refreshed `target/debug` binaries from
  `cargo build -p ozma-softnode` (4 s, in-tree target cache).

## 10. Remaining privileged gaps

1. **No node service was started.** Starting `ozma-node@hardware.service` (or a
   desktop user unit) is the next gate; it was intentionally out of scope here.
2. **udev was not reloaded/triggered.** Rules are syntax-verified but not
   applied to the running system; first application is the next boot/trigger.
3. **AppArmor drop-in not parsed/loaded** (parser and service absent).
4. **libvirt snippet not merged** into `qemu.conf`; no libvirt restart.
5. **USB gadget ConfigFS/UDC path not exercised**; only assets installed.
6. **Release profile not installed** (debug binaries), and no live capture,
   input, PipeWire, HID, or QMP data path was exercised.
7. **Host is a dirty working tree** (`5ae55e7` + uncommitted AIDA64/display
   work), so this evidence is for the tree as built, not a frozen commit.

## Verifier follow-up

`scripts/verify-system-install.sh` initially reported 81 passed / 1 failed
because its live-root dry run used `--profile hardware` on a host already
installed with `--usb-gadget`, so the existing `libcomposite` modules-load line
was treated as a profile mismatch. The live check now uses `--dry-run --force`
(still writing nothing), which is the correct way to prove dry-run safety on an
already-installed host. Re-run as the normal user:
**82 passed, 0 failed, 1 skipped (AppArmor) — RESULT: PASS**. Running the
verifier as root is not supported because it probes the user systemd manager.

