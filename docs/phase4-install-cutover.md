# Phase 4 install/runtime cutover mechanics

Status: **P0 install/runtime defaults are cut over; Phase 4 is not complete.**
The top-level installer, demo, recorder, Docker assets, and Gate 2 defaults now
use the Rust node. The controller, its demo tests, and the WebRTC helper are
still Python, and this change does not delete any Python tree.

- Date: 2026-11-25 (Australia/Sydney)
- Tree: working checkout at `04d6313` plus parallel workstream edits; this
  change is uncommitted by design.
- Scope: `install.sh`, `scripts/install-rust.sh`, `demo/run_demo.sh`,
  `demo/record.py`, `Dockerfile`, `docker-compose.yml`,
  `scripts/gate2/*`, systemd env examples, the WebRTC helper relocation, and
  this document.
- Not changed here: `docs/parity-matrix.md`, `docs/plan-execution-status.md`,
  any CI/build workflow, any Python tree deletion.
- Related audit: [`phase4-cutover-audit.md`](phase4-cutover-audit.md) (P0
  blockers 4, 7, and the staged plan in §11).

## 1. What changed

### 1.1 Top-level installer

`install.sh` now installs the Rust node by default:

| Before | After |
|---|---|
| Header: "Ozma Controller + Agent installer" | "Ozma Controller + Rust node installer" |
| Downloaded `ozma-agent-linux-<arch>.AppImage` from GitHub releases | Builds/reuses `ozma-node` + `ozma-virtual-node` via `scripts/install-rust.sh` |
| Wrote/enabled `ozma-agent.service` | Writes/enables `ozma-node@<profile>.service` (`desktop` by default) |
| Told users `uv pip install ozma-softnode` | Prints `ozma-node --profile desktop`, `ozma-virtual-node`, and the installed WebRTC helper path |
| No dry-run | `--dry-run` prints every action and writes nothing |
| No escape hatch | `--legacy-python` / `OZMA_INSTALL_LEGACY_PYTHON=1` keeps the old AppImage + Python guidance for one release |

The controller remains Python: the installer still checks Python 3.11+, creates
`~/.ozma/.venv`, and enables `ozma-controller.service` with
`ExecStart=$VENV/bin/python main.py` (`install.sh:132-149`, `install.sh:216`,
`install.sh:253`).

Node install details (`install.sh:273-316`):

- `--node-profile desktop|hardware|vm` (default `desktop`).
- Calls `scripts/install-rust.sh` with `--prefix ~/.local`,
  `--bin-dir ~/.local/bin`, and `--units-dir ~/.config/systemd/user`.
- Uses `--from-target target/release` when both binaries exist, otherwise
  `--build` when `cargo` is available; fails loudly with remediation if neither.
- Enables `ozma-node@<profile>.service` only after the binaries/units install.
- Detects an existing `ozma-agent.service`, warns, and leaves it running. It is
  not disabled automatically, so an existing install is not silently broken.

Legacy path (`install.sh:320-378`) reproduces the previous behaviour:
architecture detection, GitHub `releases/latest`, AppImage download, and
`ozma-agent.service`. It is reached only with the explicit flag/env var.

### 1.2 Demo and recorder

| Asset | Before | After |
|---|---|---|
| `demo/run_demo.sh` | `DEMO_BACKEND="${OZMA_DEMO_BACKEND:-python}"`; `--rust` opt-in | Default `rust` (`demo/run_demo.sh:43`); `--python` and `OZMA_DEMO_BACKEND=python` are the escape hatch |
| `demo/run_demo.sh` help | `--help` would start the demo | `usage()` handles `-h/--help` before any service starts; `demo/run_demo.sh:35` |
| `demo/run_demo.sh` WebRTC | Never wired | Opt-in `--webrtc` / `OZMA_DEMO_WEBRTC=1` passes `--webrtc-helper python3 --webrtc-helper-script scripts/webrtc/gst_webrtc_worker.py` (`demo/run_demo.sh:90`, `:142-160`) |
| `demo/record.py` | Always spawned `softnode/soft_node.py` | Default `--backend rust` spawns `ozma-node --profile vm`; `--backend python` keeps the old path (`demo/record.py:40`, `:84-140`, `:310`) |

The demo still starts the Python controller (`demo/run_demo.sh:294`) and
`demo/run_demo.sh test` still runs the Python E2E scripts
(`demo/run_demo.sh:215`, `:220`). Those are called out in §4.

### 1.3 Docker assets

- `Dockerfile` no longer copies `softnode/`; the image is controller-only and
  nodes run as Rust binaries on their hosts. `CMD ["python", "main.py",
  "--virtual-only"]` is unchanged because the controller is Python.
- `docker-compose.yml` quick-start now documents
  `scripts/install-rust.sh --build` + `ozma-node --profile desktop` and
  `ozma-virtual-node`, with the Python command confined to a labelled
  one-release rollback comment (`docker-compose.yml:18`).

### 1.4 Systemd

The checked-in units were already Rust-only and are unchanged:

- `scripts/systemd/ozma-node@.service:11` — `ozma-node --profile %i`
- `scripts/systemd/ozma-node-vm@.service:10` — `ozma-node --profile vm --name %i`
- `scripts/systemd/ozma-virtual-node.service:11` — `ozma-virtual-node --node-bin ...`

`scripts/install-rust.sh` now also installs the WebRTC helper and prints the
`--webrtc-helper*` flags; the desktop/VM env examples document the optional
`OZMA_WEBRTC_HELPER` / `OZMA_WEBRTC_HELPER_SCRIPT` pair. No checked-in
node/agent unit executes Python; the two Python helper units that remain are
listed in §4 (`softnode/install/ozma-evdev.service:13` and
`proxmox-plugin/debian/ozma-display@.service:10`).

Note: the current tree already forwards HLS/display/WebRTC/VNC/USB child flags
in `ozma-softnode/src/supervisor.rs` (`ChildOptions` at `:304-311`,
`build_child_argv` HLS block from `:744`). The audit's earlier
`build_node_spec`-only gap no longer applies at this commit, though that work is
outside this change and is not re-validated here.

### 1.5 WebRTC helper relocation

The implementation moved from `softnode/gst_webrtc_worker.py` to
`scripts/webrtc/gst_webrtc_worker.py` (git rename). It is still Python and is
not rewritten in Rust. References updated:

- `scripts/install-rust.sh:173-181` installs it to
  `<prefix>/share/ozma/webrtc/gst_webrtc_worker.py`, with `--no-helper` and
  `--helper-dir` controls.
- `scripts/gate2/qemu-preflight.sh:326-338` defaults to the canonical path and
  falls back to the legacy `softnode/` path.
- `scripts/gate2/README.md:39,83`, `scripts/systemd/env/*.example`,
  `ozma-node/src/webrtc.rs:11,125`, and `ozma-node/src/controller_compat.rs:5125`
  point at the canonical path.
- `softnode/gst_webrtc.py:31-44` resolves
  `$OZMA_WEBRTC_WORKER` → repo copy → `<prefix>/share/ozma/webrtc/` →
  `~/.local/share/ozma/webrtc/` → legacy in-tree path.

`softnode/gst_webrtc_worker.py` remains as a compatibility shim:

- `import gst_webrtc_worker` re-exports the canonical module (so
  `softnode/gst_webrtc_worker_test.py` and `softnode/gst_webrtc.py` still work);
- running it as a script `runpy`s the canonical file;
- lookup order is `$OZMA_WEBRTC_WORKER`, `<repo>/scripts/webrtc/...`,
  `<sys.prefix>/share/ozma/webrtc/...`, `$HOME/.local/share/ozma/webrtc/...`,
  then a legacy in-tree implementation if a rollback checkout restored one;
- if none exists it exits 2 with remediation instead of failing silently.

**Packaging caveat (honest gap):** the `ozma-softnode` wheel currently ships
the shim (setuptools auto-discovers top-level modules under `softnode/`) but not
`scripts/webrtc/`. A pip-installed Python softnode therefore cannot find the
canonical helper unless the helper is installed separately by
`scripts/install-rust.sh` (or `OZMA_WEBRTC_HELPER_SCRIPT` points at a copy).
WebRTC was already optional/deprecated on that wheel. Fix options for a later
change: ship the helper as package data at
`<site-packages>/share/ozma/webrtc/`, or copy it into `softnode/` during the
wheel build. This is documented rather than silently changed because it touches
release packaging.

## 2. Exact commands

### 2.1 Before → after

```bash
# Install (before)
curl -sSL https://ozma.dev/install.sh | bash
# installed the ozma-agent AppImage and printed:
#   uv pip install ozma-softnode
#   ozma-softnode --name my-desktop

# Install (after, default)
bash install.sh                     # controller + Rust node desktop + user unit
bash install.sh --dry-run           # verify without changing the host

# Other node profiles
bash install.sh --node-profile vm
bash install.sh --node-profile hardware

# Controller only
bash install.sh --skip-node

# One-release rollback installer path
bash install.sh --legacy-python
OZMA_INSTALL_LEGACY_PYTHON=1 bash install.sh
```

```bash
# Demo (before)
bash demo/run_demo.sh               # Python soft nodes
bash demo/run_demo.sh --rust        # Rust opt-in

# Demo (after)
bash demo/run_demo.sh               # Rust ozma-node --profile vm
bash demo/run_demo.sh --webrtc      # Rust + WebRTC helper
bash demo/run_demo.sh --python      # one-release rollback
OZMA_DEMO_BACKEND=python bash demo/run_demo.sh
bash demo/run_demo.sh --help        # prints usage, starts nothing
```

```bash
# Recorder (before)
python demo/record.py --start-services

# Recorder (after)
python demo/record.py --start-services                    # Rust nodes
python demo/record.py --start-services --backend python   # rollback
```

```bash
# Rust installer, including the helper (new)
scripts/install-rust.sh --build
scripts/install-rust.sh --build --no-helper
scripts/install-rust.sh --dry-run --build
```

```bash
# WebRTC helper (before)
--webrtc-helper-script softnode/gst_webrtc_worker.py

# WebRTC helper (after)
--webrtc-helper-script scripts/webrtc/gst_webrtc_worker.py
# installed copy:
--webrtc-helper-script "$HOME/.local/share/ozma/webrtc/gst_webrtc_worker.py"
```

### 2.2 One-command verification (dry run)

```bash
bash install.sh --dry-run           # must print the Rust node + helper plan
bash install.sh --help              # must document --dry-run/--legacy-python
bash demo/run_demo.sh --help        # must say Rust is the default
scripts/install-rust.sh --help      # must document helper install/--no-helper
python3 demo/record.py --help       # must show --backend {rust,python}
```

Expected markers in `install.sh --dry-run` output:

```
DRY   bash .../scripts/install-rust.sh --build --prefix ~/.local ...
DRY   systemctl --user enable --now ozma-node@desktop.service
DRY   webrtc helper: ~/.local/share/ozma/webrtc/gst_webrtc_worker.py
```

## 3. Legacy escape hatch and rollback

### 3.1 Runtime rollback without reverting the commit

```bash
# 1. Stop the Rust node unit (keep the controller running).
systemctl --user disable --now ozma-node@desktop.service

# 2. Re-install the deprecated agent/AppImage path.
bash install.sh --legacy-python

# 3. Or keep Rust installed and only run the Python demo:
bash demo/run_demo.sh --python
python demo/record.py --start-services --backend python

# 4. Old helper path still works through the shim:
--webrtc-helper-script softnode/gst_webrtc_worker.py
```

### 3.2 Full release rollback

The audit's tag strategy is still the source of truth
(`phase4-cutover-audit.md` §10): check out
`rust-cutover-<version>-python-default`, then reinstall that tag's installer
and units. The Rust units do not reference Python, so a source-only checkout is
not enough; rerun the tag's `install.sh` (or the prior package) before
re-enabling services.

### 3.3 Upgrading an existing install

- An existing `ozma-agent.service` is detected and left running. Disable it
  explicitly when switching:
  `systemctl --user disable --now ozma-agent`.
- `scripts/install-rust.sh` refuses to overwrite differing units/binaries
  without `--force`. Allow, then re-run; do not pass `--force` blindly if the
  operator may have edited a unit.
- The old AppImage stays in `~/.local/bin/ozma-agent` until removed manually.
  This change does not delete it.

## 4. Remaining Python references (honest inventory)

### 4.1 Intentional, in the changed files

| File:line | Reference | Why it remains |
|---|---|---|
| `install.sh:132-149` | Python 3.11+ preflight | Controller is still Python. |
| `install.sh:216-220` | `python3 -m venv ~/.ozma/.venv` | Controller virtualenv. |
| `install.sh:253` | `ExecStart=$VENV/bin/python main.py` | Controller systemd unit. |
| `install.sh:18,28,320-378` | `--legacy-python`, `ozma-agent` AppImage | Explicit one-release escape hatch. |
| `install.sh:405-406` | `uv pip install ozma-softnode` | Printed only on the legacy path. |
| `install.sh:425` | `--webrtc-helper python3` | Helper is Python by design. |
| `demo/run_demo.sh:43-48,56,163` | `--python` / `soft_node.py` | Explicit one-release rollback. |
| `demo/run_demo.sh:215,220` | `tests/test_e2e_*.py` under `$PYTHON` | `demo/run_demo.sh test` is still the Python harness. |
| `demo/run_demo.sh:241,263-270` | venv + controller deps | Python controller. |
| `demo/run_demo.sh:294` | `controller/main.py` | Python controller. |
| `demo/run_demo.sh:334` | `python demo/switch.py` | Demo helper script. |
| `demo/record.py:40-105,129,310` | `soft_node.py`, `--backend python` | Explicit rollback; default is Rust. |
| `Dockerfile:18,33,55,58` | `python:3.13-slim`, `python3-dev`, healthcheck, `CMD python main.py` | Controller image. |
| `docker-compose.yml:18` | `uv pip install ozma-softnode` | Labelled legacy comment. |
| `scripts/install-rust.sh:7-8,275,288` | "Python trees remain the rollback path" | Informational. |
| `scripts/gate2/qemu-preflight.sh:336` | legacy `softnode/` helper fallback | Only when canonical path is absent. |
| `scripts/gate2/README.md:83` | legacy fallback note | Documentation. |
| `softnode/install/install.sh:1-16` | legacy host-config installer | Superseded by `scripts/install-system.sh`; retained for rollback only. |
| `softnode/install/ozma-evdev.service:13` | `python3 .../evdev_service.py` | Legacy evdev daemon; Rust replacement is `ozma-softnode --provision-evdev`. Marked legacy in the unit header. |
| `softnode/gst_webrtc_worker.py:1-107` | compatibility shim | One-release import/exec compatibility. |
| `softnode/gst_webrtc.py:31-44` | helper lookup with legacy fallback | Python softnode rollback. |
| `scripts/webrtc/gst_webrtc_worker.py` | GStreamer worker | Python by design; not rewritten. |

### 4.2 Outside this change (must be removed or ported before deletion)

These are unchanged and were left alone deliberately; changing CI/build files
would risk the current release workflow.

| File:line | Reference |
|---|---|
| `.github/workflows/ci.yml:47` | `ruff check controller/ node/ softnode/ agent/` |
| `.github/workflows/ci.yml:100` | `python -m py_compile controller/*.py node/*.py softnode/*.py` |
| `.github/workflows/build.yml:14` | wheels matrix `[controller, softnode, agent]` |
| `.github/workflows/build.yml:49-90` | PyInstaller macOS/Windows `ozma-agent` installers |
| `.github/workflows/build.yml:172-185` | release artifacts include `ozma-agent` binaries |
| `.github/workflows/release.yml:3-82` | whole workflow builds/releases Rust `ozma-agent` AppImage/deb |
| `dev/windows-vm/provision.sh:343` | starts `softnode/soft_node.py` |
| `dev/riscv-node/connect-to-vms.sh:92,198-199` | `pgrep -f soft_node.py` |
| `dev/scripts/build-controller-usb.sh:310-311` | prints `uv pip install ozma-softnode` |
| `dev/scripts/build-installer-usb.sh:526-527` | prints `uv pip install ozma-softnode` |
| `proxmox-plugin/debian/ozma-display@.service:10` | `ExecStart=/usr/lib/ozma-proxmox/display-service.py %i` |
| `tests/conftest.py:16` | adds `softnode`, `node`, `agent` to `sys.path` |
| 24 files under `tests/` | import/start Python softnode/node/agent or multiseat (see audit §5) |
| `softnode/`, `node/`, `agent/` | the complete Python implementations, untouched |

Historical reports/audits under `docs/` still name the old helper path; the
shim keeps those commands working. `docs/parity-matrix.md` and
`docs/plan-execution-status.md` are owned by another session and were not
edited.

## 5. Validation evidence

No real install, no sudo, no network download, no systemd change, no commit.

```text
$ bash -n install.sh
$ bash -n demo/run_demo.sh
$ bash -n scripts/install-rust.sh
$ bash -n softnode/install/install.sh
$ sh -n scripts/gate2/qemu-preflight.sh
(all exit 0)

$ python3 -m py_compile \
      scripts/webrtc/gst_webrtc_worker.py \
      softnode/gst_webrtc_worker.py \
      softnode/gst_webrtc.py \
      demo/record.py
(exit 0)

$ (cd softnode && python3 gst_webrtc_worker_test.py)
Ran 38 tests in 0.004s
OK

$ bash install.sh --help
(prints Rust-default usage, --dry-run, --legacy-python)

$ OZMA_DIR=/tmp/opencode/ozma-dryrun-nonexistent bash install.sh --dry-run
DRY   git clone --depth 1 --branch main https://github.com/ozmalabs/ozma.git ...
DRY   bash .../scripts/install-rust.sh --build --prefix /home/matt/.local \
          --bin-dir /home/matt/.local/bin --units-dir /home/matt/.config/systemd/user
DRY   systemctl --user enable --now ozma-node@desktop.service
DRY   webrtc helper: /home/matt/.local/share/ozma/webrtc/gst_webrtc_worker.py
(dry run complete; no files written)

$ OZMA_DIR=/tmp/opencode/ozma-dryrun-nonexistent bash install.sh --dry-run --legacy-python
DRY   curl .../ozma-agent-linux-x86_64.AppImage ...
DRY   write .../ozma-agent.service
DRY   systemctl --user enable --now ozma-agent

$ bash demo/run_demo.sh --help
(prints "Rust ozma-node (default)" and "--python" rollback; no services start)

$ bash scripts/install-rust.sh --dry-run \
      --from-target /tmp/opencode/ozma-rust-stub \
      --prefix /tmp/opencode/ozma-rust-dry
  webrtc:      /tmp/opencode/ozma-rust-dry/share/ozma/webrtc/gst_webrtc_worker.py
DRY   install -m 0644 .../scripts/webrtc/gst_webrtc_worker.py \
          /tmp/opencode/ozma-rust-dry/share/ozma/webrtc/gst_webrtc_worker.py
(... dry run; no files written)

$ python3 demo/record.py --help
(prints --backend {rust,python}; default rust)

$ python3 softnode/gst_webrtc_worker.py --help
(executes the canonical worker through the shim; usage printed)

$ cp softnode/gst_webrtc_worker.py /tmp/opencode/ozma-wheel-shim/ && \
  python3 /tmp/opencode/ozma-wheel-shim/gst_webrtc_worker.py --help
error: the canonical GStreamer WebRTC worker was not found.
(exit 2; documents the wheel packaging gap)
```

Not run (explicitly out of scope / environment):

- a real `install.sh` run (would clone, build, use sudo, and start units);
- a real `scripts/install-rust.sh --build` (release builds are expensive);
- a live `demo/run_demo.sh` (needs QEMU VMs and the Python controller deps);
- `docker build` / `docker compose config`;
- any systemd unit enable/disable/start;
- `cargo test` (comments/strings only changed in Rust; see the audit baseline
  for the last green suite).

## 6. What is still Python-only (residual risk)

1. **Controller** — `controller/` is Python and is installed/run as such. This
   change does not cut the controller over.
2. **Demo tests** — `demo/run_demo.sh test` runs the Python E2E suite against
   the controller regardless of node backend.
3. **WebRTC helper** — `scripts/webrtc/gst_webrtc_worker.py` is Python and
   needs system Python with `gi`/GStreamer. The demo only wires it with
   `--webrtc`; without it nodes answer the documented typed 503.
4. **Recorder** — `demo/record.py` itself is a Python Playwright script
   (recording tooling, not a node runtime).
5. **Packaging** — the `ozma-softnode` wheel does not ship
   `scripts/webrtc/`; use the installed copy or `OZMA_WEBRTC_HELPER_SCRIPT`.
   Python softnode WebRTC remains an overlap-only path.
6. **Unchanged CI/build/dev/test references** — §4.2. They keep the Python
   trees alive until the deletion stage.
7. **Legacy installer path** — `--legacy-python` downloads the deprecated
   `ozma-agent` AppImage. It is opt-in only and should be removed with the
   Python trees after the overlap release.

## 7. Privileged install gate (2026-11-25)

§5 records the original uncommitted install/runtime cutover change, which
deliberately used no sudo and ran no real install. The separate privileged gate
has since been run on the same host at `5ae55e7` + uncommitted work:

```bash
./scripts/install-system.sh --dry-run --profile all --usb-gadget
./scripts/install-rust.sh --from-target target/debug --prefix "$HOME/.local"
sudo -n scripts/install-rust.sh --from-target target/debug --system --prefix /usr/local
sudo -n scripts/install-system.sh --profile all --usb-gadget
```

Both privileged commands exited 0, the second `install-system.sh` run was
idempotent, and no unit was enabled or started. `scripts/systemd/verify-units.sh`
(8/8), `systemd-analyze verify` on the installed units, `udevadm verify` (6/6),
and the tmpfiles dry run passed. `scripts/verify-system-install.sh` reported
`81 passed, 1 failed, 1 skipped`: the failure is its live-root
`--dry-run --profile hardware` check refusing the `libcomposite` line installed
by `--usb-gadget` (the same dry run with `--profile all --usb-gadget` exits 0),
not an install defect. AppArmor was skipped because the parser/service are
absent. The install was left in place; full evidence, the debug-binary and
env-ownership deviations, rollback, and remaining privileged gaps are in
[`phase4-privileged-install-report.md`](phase4-privileged-install-report.md)
and [`clean-host.md`](clean-host.md#privileged-install-gate-2026-11-25).
