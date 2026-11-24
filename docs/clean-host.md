# Clean-host Phase 4 harness

Status: **harness, Rust-first systemd assets, install layout, and the demo
Rust switch are implemented. Gate 4 is not yet met**: the container harness
builds and smoke-tests, but its bounded node test run is red on in-progress
product code from parallel workstreams (see
[Known blocker](#known-blocker-moving-tree)), the fresh-host
install has not been run end-to-end, and the Python trees are intentionally
still present for one release.

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
7. Starts `ozma-node --profile desktop` with synthetic capture/recording input
   and requires `/health` to return `ok: true`.

By default steps 1–7 run inside a rootless Podman or Docker container built
from `scripts/clean-host/Containerfile`. The host checkout is mounted read-only
in spirit (the harness only writes `target/clean-host`); no host package, unit,
user, or Python tree is changed.

## Prerequisites

- Linux x86_64 or aarch64 with a Rust toolchain new enough for the workspace
  (CI uses the `rust:1.95` image).
- Rootless Podman or Docker for the default mode. `podman` is preferred.
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

## Rust-first systemd assets

Files live in `scripts/systemd/`:

- `ozma-node@.service` — `%i` is `hardware`, `vm`, or `desktop`; env file
  `ozma-node-%i.env`.
- `ozma-node-vm@.service` — `%i` is a VM/node name (`--profile vm --name %i`).
- `ozma-virtual-node.service` — discovery/supervisor process.
- `env/*.env.example` — one example per profile plus the supervisor.
- `verify-units.sh` — renders placeholders to a temporary directory and runs
  `systemd-analyze verify`; no install required.

Validate:

```bash
scripts/systemd/verify-units.sh
# PASS  ozma-node@.service
# PASS  ozma-node-vm@.service
# PASS  ozma-virtual-node.service
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
~/.local/lib/systemd/user/ozma-node@.service
~/.local/lib/systemd/user/ozma-node-vm@.service
~/.local/lib/systemd/user/ozma-virtual-node.service
~/.local/etc/ozma/*.env.example
```

It never enables or starts a unit and never deletes `softnode/`, `node/`,
`agent/`, or any Python virtualenv. System layout is an explicit
`--system --prefix /usr/local` run by an administrator; the script never invokes
sudo. See `scripts/systemd/README.md` for user creation, state-directory
ownership, udev/group notes, and why `--provision*` stays off by default.

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

## Known blocker (moving tree)

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
scripts/systemd/verify-units.sh          # PASS (3 units, systemd 262)
scripts/clean-host/run.sh --check-deps   # PASS (host; shellcheck SKIP)
scripts/install-rust.sh --dry-run ...    # exit 0, no files written
./target/debug/ozma-node --help          # exit 0
./target/debug/ozma-virtual-node --help  # exit 0
```
