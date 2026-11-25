# Clean-host Phase 4 harness

Status: **harness, Rust-first systemd assets, user and system install layouts,
the host integration surface (user/group, udev, AppArmor, libvirt snippet), and
the demo Rust switch are implemented. Gate 4 is not yet met**: the privileged
fresh-host install has not been run end-to-end, the controller is still Python,
and the Python trees are intentionally still present for one release. The
2026-11-25 refresh at `437468a` passed the host harness, workspace tests, and
controller pytest. On 2026-11-25 the container gate also passed:
`scripts/clean-host/run.sh --container --full` at `08393e7` reported
2469 passed / 0 failed / 42 ignored (see
[`phase4-container-gate-report.md`](phase4-container-gate-report.md)). See
[`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16 for the current
go/no-go.

This page is the operational companion to
[`rust-reconciliation-plan.md`](rust-reconciliation-plan.md) Phase 4 and
[`plan-execution-status.md`](plan-execution-status.md).

## What the harness proves

`scripts/clean-host/run.sh` does all of the following without sudo and without
installing anything globally:

1. Checks the native build dependencies (`pkg-config`, libudev, ALSA,
   PipeWire, libclang/clang) and reports optional runtime tools separately.
2. Builds `ozma-node` and `ozma-virtual-node`.
3. Runs a bounded test subset (`cargo test -p ozma-node --lib` and
   `cargo test -p ozma-softnode`) or the full `cargo test --workspace` with
   `--full`.
4. Verifies both binaries answer `--help`; `ozma-node` also answers
   `--version` (`ozma-virtual-node` currently declares no clap version).
5. Runs shell syntax checks and, when `systemd-analyze` is available, verifies
   the rendered unit files.
6. Runs `scripts/install-rust.sh --dry-run` and checks it writes nothing.
7. Renders the system install surface (user/group commands, units, env examples,
   udev, AppArmor, libvirt snippet, tmpfiles, modules-load, USB-gadget opt-in)
   into a temporary root with `scripts/verify-system-install.sh`; it checks
   profile selection, idempotence, `--force`, file modes, no secrets or
   world-writable paths, and udev/AppArmor syntax when those tools exist.
8. Starts `ozma-node --profile desktop` with synthetic capture/recording input
   and requires `/health` to return `ok: true`.

By default steps 1–7 run inside a rootless Podman or Docker container built
from `scripts/clean-host/Containerfile`. The host checkout is mounted read-only
in spirit (the harness only writes `target/clean-host`); no host package, unit,
user, or Python tree is changed.

## Prerequisites

- Linux x86_64 or aarch64 with a Rust toolchain new enough for the workspace
  (CI uses the `rust:1.95` image).
- Rootless Podman or Docker for the default mode. `podman` is preferred.
- Network access (DNS plus `deb.debian.org`) for the first image build: the
  `Containerfile` runs `apt-get update` and installs the native packages inside
  the image. A pre-built `localhost/ozma-clean-host:1.95` can be reused offline.
- About 15 GB free in `target/clean-host` for a fresh container build; the
  host's existing `target/` is not reused because the container is Debian
  bookworm while many dev hosts are newer.
- For `--host`, the build packages below.
- For the demo, QEMU, ffmpeg, the Python controller dependencies, and
  optionally `pactl`.

### Build packages

| Dependency | Why | Debian/Ubuntu | Fedora/RHEL | Arch |
|---|---|---|---|---|
| `cargo`/`rustc` | build | rustup or distro | rustup or distro | rustup or `rust` |
| C toolchain | `cc` crates | `build-essential` | `gcc gcc-c++` | `base-devel` |
| `pkg-config` | native discovery | `pkg-config` | `pkgconf-pkg-config` | `pkgconf` |
| libudev | udev/hotplug, USB gadget | `libudev-dev` | `systemd-devel` | `systemd-libs` |
| ALSA | audio capture/playback | `libasound2-dev` | `alsa-lib-devel` | `alsa-lib` |
| PipeWire | audio/video capture | `libpipewire-0.3-dev` | `pipewire-devel` | `pipewire` |
| PAM | node link-time auth | `libpam0g-dev` | `pam-devel` | `pam` |
| libclang/clang | `bindgen` for `v4l2-sys-mit` | `libclang-dev clang` | `clang clang-devel` | `clang` |
| libxdo | `enigo` link dep for `ozma-hid` tests | `libxdo-dev` | `libxdo-devel` | `xdotool` |
| cmake/nasm | bundled codec deps | `cmake nasm` | `cmake nasm` | `cmake nasm` |
| ffmpeg | HLS/VNC/X11 runtime | `ffmpeg` | `ffmpeg` | `ffmpeg` |

The harness never runs `apt`, `dnf`, or `pacman`. The container image installs
these packages inside the image only.

## Exact commands

```bash
# Dependency report only (no build, no install)
scripts/clean-host/run.sh --check-deps

# Default: build + bounded tests + help + start smoke in a rootless container
scripts/clean-host/run.sh

# Force the container runtime / force host mode
scripts/clean-host/run.sh --container
scripts/clean-host/run.sh --host

# Full workspace test suite (long)
scripts/clean-host/run.sh --full

# Skip the start/health smoke (help + build + tests only)
scripts/clean-host/run.sh --no-smoke

# Rebuild the image / remove image and target cache afterwards
scripts/clean-host/run.sh --rebuild
scripts/clean-host/run.sh --clean
```

Logs go to `/tmp/ozma-clean-host-<uid>/logs/` (`run.log`,
`container-build.log`, or `host-run.log`). The final line is `RESULT: PASS` or
`RESULT: FAIL`; individual checks print `PASS`/`FAIL`/`SKIP`.

### Expected results

A green host prints, in order:

- `PASS` for each required package and `SKIP` for missing optional tools.
- `PASS  cargo build -p ozma-node -p ozma-softnode --bins`.
- `PASS  ozma-node --help (contains --profile)` and
  `PASS  ozma-virtual-node --help (contains --node-bin)`.
- `PASS` for every `bash -n`, the unit verification, and the dry-run installer.
- `PASS  cargo test -p ozma-node --lib` and
  `PASS  cargo test -p ozma-softnode`.
- `PASS  ozma-node desktop profile answered /health on port ...`.
- `RESULT: PASS`.

If the only failure is a native dependency, install the matching package and
rerun. If no container runtime is available, the harness reports the exact
missing runtime and falls back to `--host` in `--auto` mode; with
`--container` it exits 1 and tells you to use `--host`.

The image is built on demand and cached. If the dependency report inside the
container says `libxdo` is missing, the cached image predates `libxdo-dev`;
rebuild it with `--rebuild` (network access is required). The rebuild adds
`libxdo3` and `libxdo-dev`; Debian ships no `xdo.pc`, so the harness detects the
library through its `/usr/lib/*/libxdo.so` fallback.

## Rust-first systemd assets

Files live in `scripts/systemd/`:

- `ozma-node@.service` — `%i` is `hardware`, `vm`, or `desktop`; env file
  `ozma-node-%i.env`.
- `ozma-node-vm@.service` — `%i` is a VM/node name (`--profile vm --name %i`).
- `ozma-virtual-node.service` — discovery/supervisor process.
- `ozma-multiseat.service` — desktop-seat supervisor; loads the persisted
  `seats.json` and runs one `ozma-node --profile desktop` per X output.
- `env/*.env.example` — one example per profile, the supervisor, and multiseat.
- `verify-units.sh` — renders placeholders to a temporary directory and runs
  `systemd-analyze verify` for both the system units and least-privilege user
  units; no install required.

`ozma-meeting-detect` has no unit: it is a per-session, on-demand IPC server that
is not yet wired to the controller report path.

Validate:

```bash
scripts/systemd/verify-units.sh
# PASS  system ozma-node@.service
# PASS  system ozma-node-vm@.service
# PASS  system ozma-virtual-node.service
# PASS  system ozma-multiseat.service
# PASS  user ozma-node@.service
# PASS  user ozma-node-vm@.service
# PASS  user ozma-virtual-node.service
# PASS  user ozma-multiseat.service

scripts/systemd/verify-units.sh --system-only
scripts/systemd/verify-units.sh --user-only
```

`systemd-analyze` absent is a SKIP, not a failure. The units are system units by
default; least-privilege notes and the user-unit path are in
`scripts/systemd/README.md`.

## Install layout (no sudo, no Python removal)

```bash
scripts/install-rust.sh --build --prefix "$HOME/.local"
# or, with already-built binaries:
scripts/install-rust.sh --from-target target/release --prefix "$HOME/.local"

# Preview only: prints actions and writes nothing
scripts/install-rust.sh --dry-run --from-target target/debug --prefix /tmp/ozma-install
```

The user layout installs:

```text
~/.local/bin/ozma-node
~/.local/bin/ozma-virtual-node
~/.local/bin/ozma-multiseat          # when built
~/.local/bin/ozma-meeting-detect     # when built; on demand, no unit
~/.local/lib/systemd/user/ozma-node@.service
~/.local/lib/systemd/user/ozma-node-vm@.service
~/.local/lib/systemd/user/ozma-virtual-node.service
~/.local/lib/systemd/user/ozma-multiseat.service   # when the binary is installed
~/.local/etc/ozma/*.env.example
```

The optional binaries are installed only when present in the target directory;
`--build` adds the `ozma-multiseat` package. `--no-multiseat` skips the
`ozma-multiseat` binary, its unit, and its env example; `--no-meeting-detect`
skips the meeting binary. Both flags compose with `--dry-run`, which prints the
exact `install`/`write`/`copy` actions and writes nothing.

It never enables or starts a unit and never deletes `softnode/`, `node/`,
`agent/`, or any Python virtualenv. System layout is an explicit
`--system --prefix /usr/local` run by an administrator; the script never invokes
sudo. See `scripts/systemd/README.md` for user creation, state-directory
ownership, udev/group notes, and why `--provision*` stays off by default.

## System install (fresh host)

`scripts/install-system.sh` completes the system-wide host integration that
`scripts/install-rust.sh` deliberately leaves to the administrator. It is
idempotent, never invokes sudo, never enables or starts a unit, and never edits
`/etc/libvirt/qemu.conf`. It installs units and env examples itself, so a host
can be staged before the binaries exist.

### Scripted vs operator-only

| Step | Scripted | How |
|---|---|---|
| `ozma-node` system user/group plus existing `video`/`input`/`render`/`kvm`/`libvirt` groups | yes, as root | `install-system.sh` |
| State/runtime/config dirs with least-privilege modes | yes, as root | `install-system.sh` |
| Units, env examples, tmpfiles and modules-load snippets | yes | `install-system.sh` |
| `ozma-multiseat.service` + env example | yes (unit/env only; binary via `install-rust.sh`) | `install-system.sh` |
| `ozma-meeting-detect` | binary only, on demand — no unit | `install-rust.sh` |
| udev rules for uinput, hidg, V4L2, render, optional gadget | yes, as root | `install-system.sh` |
| AppArmor libvirt-qemu drop-in | yes, as root | `install-system.sh` |
| libvirt `cgroup_device_acl` | **no** — copy-only snippet | merge into `qemu.conf` |
| Enable/start a unit | **no** — exact commands are printed | `systemctl enable --now ...` |
| USB gadget ConfigFS setup / UDC binding | **no** — opt-in assets and guidance | operator |
| Real host validation | **no** on this checkout | run on a fresh host |

### Runtime packages

Build packages are in [Build packages](#build-packages). The system integration
adds these runtime packages:

| Dependency | Why | Debian/Ubuntu | Fedora/RHEL | Arch |
|---|---|---|---|---|
| systemd/udev | units, udev, tmpfiles | `systemd udev` | `systemd` | `systemd` |
| AppArmor (optional) | QEMU evdev access | `apparmor` | `apparmor` | `apparmor` |
| libvirt/QEMU (optional) | VM profile and input-linux | `libvirt-daemon-system qemu-system-x86` | `libvirt qemu-kvm` | `libvirt qemu-full` |
| PipeWire/PulseAudio tools | desktop/VM audio routing | `pipewire pulseaudio-utils` | `pipewire pulseaudio-utils` | `pipewire libpulse` |
| ffmpeg | HLS/VNC/X11 capture | `ffmpeg` | `ffmpeg` | `ffmpeg` |
| kmod | modules-load snippet | `kmod` | `kmod` | `kmod` |
| shadow/passwd | `useradd`/`groupadd` | `passwd` | `shadow-utils` | `shadow` |

### Exact fresh-host commands

```bash
# 0. Build or copy the binaries (run as root; install-rust.sh never uses sudo)
scripts/install-rust.sh --build --system --prefix /usr/local

# 1. Preview the host integration (writes nothing)
scripts/install-system.sh --dry-run --profile hardware

# 2. Install it (run as root)
scripts/install-system.sh --profile hardware
# every profile plus the USB-gadget opt-in:
# scripts/install-system.sh --profile all --usb-gadget

# 3. Rootless review of the exact rendered layout (no root, temp root)
scripts/verify-system-install.sh --keep

# 4. Read the exact skipped/host commands, then edit the env file
cat /etc/ozma/install-operator-commands.txt
$EDITOR /etc/ozma/ozma-node-hardware.env
chmod 0600 /etc/ozma/ozma-node-hardware.env   # once it contains a token

# 5. Apply host state (the installer does not do this)
systemctl daemon-reload
udevadm control --reload-rules
udevadm trigger --action=change
systemd-tmpfiles --create /etc/tmpfiles.d/ozma-node.conf
systemctl reload apparmor        # only if AppArmor is active

# 6. Merge the libvirt snippet only if you use QEMU input-linux
$EDITOR /etc/libvirt/qemu.conf   # add or extend cgroup_device_acl
systemctl restart libvirtd

# 7. Enable a profile (the installer never enables or starts anything)
systemctl enable --now ozma-node@hardware.service
# or, on a hypervisor:
systemctl enable --now ozma-virtual-node.service
# or, for desktop seats (edit /etc/ozma/ozma-multiseat.env first):
systemctl enable --now ozma-multiseat.service
```

`install-rust.sh --system` installs the binaries, units, and env examples;
`install-system.sh` re-renders the same units/env and adds the host assets. Run
`install-rust.sh` first so `/usr/local/bin/ozma-node` exists before a unit
starts. Either order is safe because both steps are idempotent.

The rendered layout is:

```text
/usr/local/lib/systemd/system/ozma-node@.service
/usr/local/lib/systemd/system/ozma-node-vm@.service
/usr/local/lib/systemd/system/ozma-virtual-node.service
/usr/local/lib/systemd/system/ozma-multiseat.service
/usr/local/lib/systemd/system/ozma-node@.service.d/10-usb-gadget.conf  # opt-in
/etc/ozma/*.env.example
/etc/ozma/install-operator-commands.txt
/etc/ozma/libvirt-qemu-cgroup_device_acl.conf
/etc/udev/rules.d/70-ozma-node-*.rules
/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
/etc/tmpfiles.d/ozma-node.conf
/etc/modules-load.d/ozma-node.conf
/var/lib/ozma-node, /var/lib/ozma, /run/ozma-node, /run/ozma, /run/ozma/qmp
```

### System user and groups

The installer creates a dedicated, non-login service account and adds only the
supplementary groups that exist on the host:

```bash
groupadd --system ozma-node
useradd --system --gid ozma-node --home-dir /var/lib/ozma-node \
    --shell /usr/sbin/nologin --no-create-home ozma-node
usermod -aG video,input,render,kvm,libvirt ozma-node   # existing groups only
```

Group selection follows the profile: `hardware` gets `video input render`;
`vm` gets `input render kvm libvirt`; `desktop` gets `input video render`.
`--profile all` takes the union. The node never runs as root and is not given
`CAP_SYS_ADMIN`. Use `--no-user` to keep account creation entirely manual.

### udev rules

| Rule | Devices | Profiles |
|---|---|---|
| `70-ozma-node-uinput.rules` | `/dev/uinput` | vm, desktop |
| `70-ozma-node-input.rules` | `/dev/input/event*`, virtual `ozma-kbd-*`/`ozma-mouse-*` | vm, desktop |
| `70-ozma-node-hidg.rules` | `/dev/hidg*` | hardware, `--usb-gadget` |
| `70-ozma-node-v4l2.rules` | `/dev/video*` | hardware |
| `70-ozma-node-render.rules` | `/dev/dri/renderD*` | all |
| `70-ozma-node-gadget.rules` | UDC/gadget class nodes | `--usb-gadget` |

Rules use the conventional `video`, `input`, and `render` groups, the
`ozma-node` group for uinput/hidg, and `kvm` (or `root` when no `kvm` group
exists) for QEMU-consumed virtual devices. `uinput` is loaded by the
modules-load snippet. The installer prints the exact reload commands and does
not run them unless `--reload` is passed.

### AppArmor and libvirt

The AppArmor drop-in lets QEMU read `/dev/input/event*` for `input-linux`:

```text
/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
```

Reload AppArmor only when it is active. If AppArmor is absent, the drop-in is
harmless and remains for a later install. The node process itself is confined by
the systemd sandbox in `ozma-node@.service`, not by this drop-in.

libvirt is never edited. The installer writes a mergeable snippet to
`/etc/ozma/libvirt-qemu-cgroup_device_acl.conf`; the operator adds it to
`/etc/libvirt/qemu.conf`, restarts libvirtd, and restarts the VMs. If
`qemu.conf` already has `cgroup_device_acl`, merge only the missing input event
entries rather than appending a second key.

QMP socket creation is separate from this installer. Use the Rust supervisor's
opt-in `--provision` path or an existing libvirt hook; neither is enabled by
default, and the installer never edits domain XML.

### USB gadget opt-in

`--usb-gadget` adds the gadget udev rule, `libcomposite` to modules-load, and
`ozma-node@.service.d/10-usb-gadget.conf` (which sets `ProtectKernelTunables=no`
and `ReadWritePaths=-/sys/kernel/config/usb_gadget`). ConfigFS entries are
created by the kernel as root; the node does not receive `CAP_SYS_ADMIN`. The
operator must mount ConfigFS, provide a hardware UDC, and arrange writable
gadget ownership or run the gadget setup as root. Set `OZMA_USB_GADGET=1`,
`OZMA_USB_UDC`, and `OZMA_USB_CONFIGFS_ROOT` in the env file (or pass the
matching `--usb-*` flags through `OZMA_NODE_ARGS`). Without a usable UDC or root
the node stays up, advertises no USB capability, and `/usb` returns a typed 503.

### Desktop profile caveat

A system unit runs as `ozma-node` and cannot see a logged-in user's PipeWire
server or `XDG_RUNTIME_DIR`. Prefer a user unit:

```bash
scripts/install-rust.sh --build --prefix "$HOME/.local"
systemctl --user daemon-reload
systemctl --user enable --now ozma-node@desktop.service
```

A system `ozma-node@desktop.service` only works if the desktop session is granted
explicitly and `ProtectHome`/`ReadWritePaths` are relaxed for the captured
files; that is an operator decision, not an installer default.

`ozma-multiseat.service` follows the same rule. It keeps `PrivateTmp=no` because
it must reach `/tmp/.X11-unix`, and a system unit cannot read a user's
`~/.Xauthority` under `ProtectHome=yes`. Use a `systemctl --user` unit for a live
desktop session, or provide a readable `XAUTHORITY` (for example
`/etc/ozma/Xauthority`) in `ozma-multiseat.env`. Seat ownership/routing/audio,
private Xvfb/Xephyr displays, and input isolation stay opt-in through
`OZMA_MULTISEAT_ARGS`; the unit never passes `--print-plan` or `--dry-run`.

### Verify the system surface

```bash
scripts/verify-system-install.sh
# ...
# 82 passed, 0 failed, 1 skipped
# RESULT: PASS
```

The check renders every profile into a temporary root, validates
`--dry-run`/`--force`/idempotence, checks modes, rejects uncommented secrets and
world-writable paths, verifies profile groups, tmpfiles/runtime-dir agreement,
the copy-only libvirt snippet, and the desktop user-unit caveat, verifies the
rendered units, and runs `udevadm verify`; AppArmor syntax is checked when
`apparmor_parser` exists and otherwise reported as SKIP. The count drops when a
tool is absent (`udevadm`, `apparmor_parser`, and `systemd-analyze` each turn
their checks into SKIPs). It never needs root and removes its temp roots unless
`--keep` is set. Full host/container evidence and the root-only follow-up steps
are in [`phase4-system-install-report.md`](phase4-system-install-report.md).

### Rollback

```bash
systemctl disable --now ozma-node@hardware.service
# repeat for any other enabled unit (ozma-node@vm, ozma-virtual-node, ...)
rm -f /usr/local/lib/systemd/system/ozma-node@.service \
      /usr/local/lib/systemd/system/ozma-node-vm@.service \
      /usr/local/lib/systemd/system/ozma-virtual-node.service \
      /usr/local/lib/systemd/system/ozma-multiseat.service
rm -rf /usr/local/lib/systemd/system/ozma-node@.service.d
rm -f /etc/udev/rules.d/70-ozma-node-*.rules
rm -f /etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
rm -f /etc/tmpfiles.d/ozma-node.conf /etc/modules-load.d/ozma-node.conf
rm -f /etc/ozma/libvirt-qemu-cgroup_device_acl.conf
# keep /etc/ozma if it contains tokens; remove it only deliberately
userdel ozma-node
groupdel ozma-node
systemctl daemon-reload
udevadm control --reload-rules
systemctl reload apparmor          # if active
# revert /etc/libvirt/qemu.conf manually if you merged the snippet, then:
systemctl restart libvirtd
```

The Python trees are untouched and remain the rollback path:
`OZMA_DEMO_BACKEND=python`, `bash demo/run_demo.sh --python`, or checking out
the rollback tag. No Python tree is removed by this install surface.

## Demo switch

Python remains the default (rollback path). The Rust path is opt-in:

```bash
# Build once
cargo build --release -p ozma-node

# Rust nodes: same QMP sockets, mDNS names, controller, and demo scenarios
bash demo/run_demo.sh --rust

# Equivalent environment switch
OZMA_DEMO_BACKEND=rust bash demo/run_demo.sh

# Reuse running VMs
bash demo/run_demo.sh --rust --no-vms

# Roll back without changing the tree
bash demo/run_demo.sh --python
```

The Rust branch starts one `ozma-node --profile vm` per demo VM with:

- `--name vm1|vm2` (the controller's `vm1._ozma._udp.local.` ids),
- `--qmp-socket /tmp/ozma-vm{1,2}.qmp`,
- HID ports 7332/7333 and HTTP ports 7382/7383 (HLS + compat API),
- QMP capture and QMP input (guest injection, no `/dev/uinput` needed),
- local HLS under `demo/logs/hls-vm{1,2}/`,
- PipeWire monitor audio only when `pactl` exists.

Binary lookup order is `$OZMA_NODE_BIN`, `target/release/ozma-node`,
`target/debug/ozma-node`, then `$PATH`. `bash demo/run_demo.sh status` prints
the selected backend. The Python controller still runs; this switch changes only
the node process.

## Rollback and tag criteria

Before changing a release default to Rust:

1. Tag the last Python-default, Gate 3-green tree, for example
   `git tag -a rust-cutover-<version>-python-rollback -m "last Python-default node tree"`.
2. Keep the Python trees for one full release. The Rust installer and units do
   not reference them, so rollback is `OZMA_DEMO_BACKEND=python`,
   `bash demo/run_demo.sh --python`, or checking out the tag.
3. Publish both install paths for that release and state the default in the
   release notes.
4. Delete `softnode/`, `node/`, `agent/`, and `src/` only after Gate 4 is green
   and the overlap release has shipped. Deletion is a separate reviewed
   change, not part of this harness.

Gate 4 evidence still required: a Python-free fresh-host install, the
controller/demo regression against the Rust nodes, and a green
`cargo test --workspace` on the release commit.

Current state (2026-11-25): only annotated `rust-cutover-preview-*` tags exist
(latest at `437468a`); none is the
`rust-cutover-<version>-python-rollback` tag. A clean detached run at `437468a`
passed `scripts/clean-host/run.sh --host` 20/20 (`ozma-node --lib` 469 passed /
3 ignored, `ozma-softnode` 125 passed), `cargo test --workspace` 2442 passed /
0 failed / 42 ignored, `cargo check --workspace --all-targets`, and controller
pytest 2579 passed. A clean detached run at `08393e7` then passed the container
gate `scripts/clean-host/run.sh --container --full` (2469 passed / 0 failed /
42 ignored; image digest
`sha256:c39f7af3fa52bc6135576b9fd03ffbdc1af2cdb90b6c348db94ee72ff0571a64`) and
`--host` 22/22 (`ozma-node --lib` 479 passed / 3 ignored, `ozma-softnode` 128
passed). The privileged install remains unrun. The `Cargo.lock` staleness
reported at `437468a` did not reproduce as a working-tree rewrite at `08393e7`,
but `--locked` was not asserted and the finding remains open. Deletion criteria,
the refreshed cutover matrix, the Gate 4a/4b split, the container evidence, and
the current go/no-go are in
[`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16 and
[`phase4-container-gate-report.md`](phase4-container-gate-report.md).

## Container gate evidence (2026-11-25)

`scripts/clean-host/run.sh --container --full` passed at `08393e7` in a rootless
Podman 6.2.0 container. The image build itself needs network access once; the
`Containerfile` already listed `libxdo-dev`, so the earlier `libxdo` failure was
a stale cached image, not a missing package. The rebuild added `libxdo3` and
`libxdo-dev` 1:3.20160805.1-5.

| Check | Result | Evidence |
|---|---|---|
| Image build (`podman build --network=host`) | **PASS** | image `d84d9288cc78`, digest `sha256:c39f7af3…571a64`, 3.38 GB, 2026-11-25T03:31:31–03:31:51Z |
| Container dependency report | **PASS** | 11 passed / 0 failed / 8 skipped (optional tools), `libxdo: found` |
| `--container --full` harness | **PASS** | 20 passed / 0 failed / 1 skipped (`systemd-analyze` absent in image), 2026-11-25T03:32:16–03:34:44Z |
| `cargo test --workspace` | **PASS** | 2469 passed / 0 failed / 42 ignored, 192 result blocks (`ozma-node --lib` 479/3, `ozma-softnode` 128) |
| `--host` comparison | **PASS** | 22 passed / 0 failed / 0 skipped, deps 17/0/2, `systemd-analyze verify` PASS, 2026-11-25T03:35:10–03:36:39Z |

Remaining issues: the first build needs container-network DNS unless the image
is pre-built; the image omits `systemd-analyze`, `udevadm`, qemu/virsh/pactl,
and shellcheck (expected `SKIP`); the privileged fresh-host install and a
`--locked` check are still outstanding; and `in-container.sh` labels the host
dependency report as `(container)` (cosmetic). Full commands, timestamps, and
cleanup are in
[`phase4-container-gate-report.md`](phase4-container-gate-report.md).

## Known blocker (moving tree)

> **Historical (superseded 2026-11-25):** this records an earlier moving-tree
> run. At `08393e7` the detached container `--full` and host harnesses are both
> green; see [Container gate evidence](#container-gate-evidence-2026-11-25).

On 2026-11-25 the container image built and the harness passed dependency,
build, `--help`, shell/unit, installer dry-run, `ozma-softnode` (111 tests), and
the synthetic `/health` start checks. `cargo test -p ozma-node --lib` ran 299
tests with 2 failures in parallel in-progress work:
`controller_compat::tests::display_select_without_a_control_is_a_typed_501`
(expected `unsupported`, got `display.console_unsupported`) and
`webrtc::tests::status_is_honest_before_and_after_a_failure`. Both reproduce on
the host and touch files being edited by the display/WebRTC workstreams
(`ozma-node/src/display_control.rs`, `controller_compat.rs`, `webrtc.rs`), not
the harness. An earlier `ozma-usb` compile failure cleared once that workstream
updated its files. No Gate 4 claim is made while the checkout's own tests are
red.

No Gate 4 claim is made while the checkout cannot build its own test targets.
Rerun on the cutover commit:

```bash
scripts/clean-host/run.sh --container          # bounded, with start smoke
scripts/clean-host/run.sh --container --full   # workspace suite
```

Independent checks that do not need the moving libraries still pass:

```bash
scripts/systemd/verify-units.sh          # PASS (4 system + 4 user, systemd 262)
scripts/verify-system-install.sh         # PASS (82 passed, 0 failed, 1 skipped)
scripts/clean-host/run.sh --check-deps   # PASS (host; shellcheck/apparmor SKIP)
scripts/install-rust.sh --dry-run ...    # exit 0, no files written
scripts/install-system.sh --dry-run ...  # exit 0, no files written
./target/debug/ozma-node --help          # exit 0
./target/debug/ozma-virtual-node --help  # exit 0
```
