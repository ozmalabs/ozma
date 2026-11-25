# Phase 4 CI / build / release cutover report

Status: **Rust-first CI/build/release is in place; Python remains the one-release
rollback and is still runnable.** No Python tree was deleted, no product Rust or
controller Python was changed, and nothing was committed.

- Date: 2026-11-25 (Australia/Sydney)
- Scope: `.github/workflows/{ci,rust,build,release,docker}.yml`,
  `Dockerfile`, `docker-compose.yml`, `.dockerignore`, `Makefile`, this report.
- Related: [`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16,
  [`phase4-install-cutover.md`](phase4-install-cutover.md). Those documents,
  `docs/parity-matrix.md`, and `docs/plan-execution-status.md` were not edited.

## 1. Job matrix

### `.github/workflows/ci.yml` — push to `main` / pull requests

| Job | Gate | What it does |
|---|---|---|
| `commitlint` | blocking (PR only) | Conventional-commit check. |
| `proto` | blocking | `buf lint`, `buf format --diff --exit-code`, `buf breaking`. |
| `lint` | advisory | `ruff check controller/ node/ softnode/ agent/ \|\| true`; comment marks the three node/agent trees legacy. |
| `typecheck` | advisory | `mypy controller/ ... \|\| true`. |
| `python-controller` | **blocking** | `pytest tests/unit/controller` with the pre-existing GraphQL/moonlight ignores. |
| `python-legacy-tests` | **optional, non-blocking** | `continue-on-error: true`; runs the rest of `tests/` with `--ignore=tests/unit/controller`. This is the Python softnode/node/agent/ecosystem suite, labelled legacy. |
| `compile-check` | **blocking** | `python -m py_compile controller/*.py` only. |
| `python-legacy-compile` | **optional, non-blocking** | `py_compile node/*.py softnode/*.py agent/*.py`. |
| `dco-check` | blocking (PR only) | DCO sign-off. |

### `.github/workflows/rust.yml` — push to `main` / pull requests

This is the authoritative Rust workspace gate.

| Job | Gate | Command |
|---|---|---|
| `fmt` | advisory | `cargo fmt --all -- --check` (see gap G1). |
| `check` | **blocking** | `cargo check --workspace --all-targets`. |
| `clippy` | advisory | `cargo clippy --workspace --all-targets -- -D warnings` (see gap G1). |
| `test` | **blocking** | `cargo build --workspace --bins` then `cargo test --workspace --no-fail-fast`. |

The Rust workspace covers `ozma-node`, `ozma-softnode` (`ozma-virtual-node`,
`ozma-meeting-detect`), `ozma-multiseat`, `ozma-usb`, `ozma-audio`, `ozma-mesh`,
`ozma-hardware`, `ozma-thermal`, and the rest of the `Cargo.toml` members.
`ubuntu-24.04` and PipeWire 1.x headers are used, matching `ozma-node.yml`.
`RUSTFLAGS=-D warnings` was removed so one pre-existing warning cannot fail the
blocking check; clippy keeps `-D warnings` but is advisory.

### `.github/workflows/build.yml` — tag `v*` / manual dispatch

| Job | Gate | Artifact / action |
|---|---|---|
| `wheel-controller` | **blocking** | `wheel-controller` (`controller-*.whl`). |
| `wheel-legacy` | optional, non-blocking | `wheel-softnode`, `wheel-agent`; one-release rollback wheels. |
| `container` | **blocking** | `ghcr.io/ozmalabs/ozma-controller:latest` and `:<tag>`. |
| `rust-binaries` (matrix x86_64/aarch64) | **blocking** | `ozma-rust-node-linux-{x86_64,aarch64}.tar.gz` + `.sha256`. |
| `webrtc-helper` | **blocking** | `ozma-webrtc-helper.tar.gz` + `.sha256`. |
| `macos-agent-legacy` | manual opt-in only, non-blocking | `legacy-ozma-agent-macos`; runs only when `legacy_python_agents=true`. |
| `windows-agent-legacy` | manual opt-in only, non-blocking | `legacy-ozma-agent-windows`; same condition. |
| `deb` | blocking | Proxmox plugin `.deb`. |
| `publish-pypi` | controller-gated; legacy wheels if built | Controller wheel is required; softnode/agent wheels upload when present. |
| `sign-ota` | tag only | Existing `OTA_SIGNING_KEY` path, unchanged. |
| `release` | tag only, needs core jobs | GitHub Release with Rust archives, helper, controller wheel, `.deb`, `.sig`. It does **not** need the legacy agent or legacy wheel jobs. |

### `.github/workflows/release.yml` — legacy only

The former `Release ozma-agent` workflow is now
`Legacy ozma-agent installers (deprecated)`:

- No `push: tags` trigger. It cannot run as part of the default release.
- `workflow_dispatch` requires `legacy_agent=true`.
- Every job is marked `[legacy]` and `continue-on-error: true`.
- Artifact names are kept (`ozma-agent-linux-*.AppImage`, `.deb`, MSI, DMG) but
  are not referenced by `build.yml`.

### Other workflows

| Workflow | Status |
|---|---|
| `.github/workflows/docker.yml` | Controller image only; image name aligned to `ghcr.io/ozmalabs/ozma-controller`, `latest` added for the default branch. |
| `.github/workflows/ozma-node.yml` | Unchanged Rust node build/test/aarch64 `.deb` workflow. |
| `.github/workflows/agent-ui.yml` | Unchanged `ozma-agent-ui` Rust app build. |

## 2. Release artifact changes

| Before | After |
|---|---|
| `wheels` matrix `[controller, softnode, agent]`, all treated the same | `wheel-controller` blocking; `wheel-legacy` non-blocking. |
| No Rust node release archive | `ozma-rust-node-linux-x86_64.tar.gz`, `ozma-rust-node-linux-aarch64.tar.gz` |
| No WebRTC helper release asset | `ozma-webrtc-helper.tar.gz` |
| macOS/Windows Python `ozma-agent` built on every tag | Manual `legacy_python_agents` input only; non-blocking; excluded from release assets. |
| Tag-triggered Rust `ozma-agent` AppImage/deb/MSI/DMG release | Manual legacy workflow only. |
| Release assets: agent installers + all wheels + deb + sig | Rust archives + helper + controller wheel + deb + sig. Legacy wheels still publish to PyPI when built. |

### Layout matches `scripts/install-rust.sh`

The Rust archive is flat and uses the exact binary names the installer reads from
`target/release`:

```text
ozma-node
ozma-virtual-node
ozma-meeting-detect
ozma-multiseat
```

Install from a release:

```bash
tar -xzf ozma-rust-node-linux-x86_64.tar.gz -C /tmp/ozma-rust-node
scripts/install-rust.sh --from-target /tmp/ozma-rust-node \
    --prefix ~/.local --bin-dir ~/.local/bin
```

The helper archive extracts to the installer's default helper directory:

```bash
tar -xzf ozma-webrtc-helper.tar.gz -C ~/.local
# ~/.local/share/ozma/webrtc/gst_webrtc_worker.py
```

This is the same path `scripts/install-rust.sh` writes
(`$PREFIX/share/ozma/webrtc/gst_webrtc_worker.py`).

## 3. Docker and local defaults

- `Dockerfile` was already controller-only (`COPY controller/`, no `softnode/`).
  It still uses `python:3.13-slim` and `CMD ["python", "main.py",
  "--virtual-only"]` because the controller remains Python.
- `.dockerignore` now excludes `target`, `node`, `softnode`, `agent`, and
  `ozma-agent`, so the controller build context cannot silently include the
  Python/Rust node trees.
- `docker-compose.yml` now pulls `ghcr.io/ozmalabs/ozma-controller:latest` and
  documents `scripts/install-rust.sh --build`, `ozma-node --profile desktop`,
  and `ozma-virtual-node`; the `uv pip install ozma-softnode` line remains only
  as a labelled one-release rollback comment (`docker-compose.yml:18`).
- `Makefile` defaults are Rust-first:
  - `make build` → `cargo build --release --bins -p ozma-node -p ozma-softnode -p ozma-multiseat`
  - `make test` → `cargo test --workspace`
  - `make check` → `cargo check --workspace --all-targets`
  - `make install-node` → `scripts/install-rust.sh --from-target target/release`
  - `make demo` → `bash demo/run_demo.sh` (Rust backend default)
  - `make demo-python` → `bash demo/run_demo.sh --python` (legacy escape hatch)
  - `make legacy-python` → prints the deprecated Python/AppImage commands
  - `make ui` → unchanged controller UI build.

## 4. Validation performed

No full CI/release job was run.

| Check | Result |
|---|---|
| `python3 -c "yaml.safe_load(...)"` on `ci.yml`, `rust.yml`, `build.yml`, `release.yml`, `docker.yml`, `docker-compose.yml` | **PASS** — all parse as mappings. |
| `needs:` graph consistency check for every job in the five workflows | **PASS** — no missing job references. |
| `cargo metadata --locked --offline` | **PASS** — exit 0 (lockfile clean). |
| `cargo fmt --all -- --check` | **FAIL (pre-existing)** — diffs such as `ozma-web/src/camera.rs:220`; therefore `fmt` remains advisory. |
| `bash -n` on `demo/run_demo.sh`, `scripts/install-rust.sh`, `scripts/clean-host/{run,in-container,check-deps}.sh` | **PASS** |
| `sh -n scripts/gate2/run-gate2.sh` | **PASS** |
| `python3 -m py_compile scripts/webrtc/gst_webrtc_worker.py` | **PASS** |
| `make -n` for `build`, `test`, `check`, `install-node`, `demo`, `demo-python`, `legacy-python`, `ui` | **PASS** — commands render as intended. |

Intended CI commands (not run here):

```bash
# Rust gate
cargo fmt --all -- --check                 # advisory
cargo check --workspace --all-targets      # blocking
cargo clippy --workspace --all-targets -- -D warnings  # advisory
cargo build --workspace --bins             # blocking
cargo test --workspace --no-fail-fast      # blocking

# Python gate
python -m pytest tests/unit/controller --timeout 30 -v -p no:cacheprovider \
  --ignore=tests/unit/controller/test_graphql_mutations.py \
  --ignore=tests/unit/controller/test_graphql.py \
  --ignore=tests/unit/controller/test_graphql_subscriptions.py \
  --ignore=tests/unit/controller/test_capture_to_moonlight.py \
  --ignore=tests/unit/controller/test_moonlight_app_mapping.py
python -m py_compile controller/*.py

# Release build (per arch)
cargo build --release --target <target> --bins \
  -p ozma-node -p ozma-softnode -p ozma-multiseat
```

## 5. Remaining Python references and CI gaps

### G1 — Rust formatting and clippy are advisory, not blocking

`cargo fmt --all -- --check` already fails on existing code (for example
`ozma-web/src/camera.rs:220`). The audit also records a pre-existing
`unused_mut` warning in `ozma-node/src/controller_compat.rs:3511`. Making either
job blocking would fail CI without a product-code change, which is out of scope
for this task. They are labelled advisory and remain visible.

### G2 — Python controller and helper are still Python

| Reference | Reason |
|---|---|
| `Dockerfile:18` | `FROM python:3.13-slim` — controller image. |
| `Dockerfile:55` | Python healthcheck. |
| `Dockerfile:58` | `CMD ["python", "main.py", "--virtual-only"]` — controller entrypoint. |
| `scripts/webrtc/gst_webrtc_worker.py` | GStreamer WebRTC helper; shipped as an asset, not rewritten in Rust. |
| `scripts/gate2/qemu-preflight.sh:47` | `OZMA_WEBRTC_HELPER=${OZMA_WEBRTC_HELPER:-python3}` — helper default. |
| `scripts/gate2/qemu-preflight.sh:336` | Legacy `softnode/gst_webrtc_worker.py` fallback when the canonical helper is absent. |

### G3 — Legacy Python jobs and trees remain on purpose

| Reference | Status |
|---|---|
| `.github/workflows/ci.yml:50` | `ruff check controller/ node/ softnode/ agent/` — advisory. |
| `.github/workflows/ci.yml:89-120` | `python-legacy-tests` — optional, non-blocking. |
| `.github/workflows/ci.yml:134-144` | `python-legacy-compile` — optional, non-blocking. |
| `.github/workflows/build.yml:39-57` | `wheel-legacy` softnode/agent — optional, non-blocking. |
| `.github/workflows/build.yml:188-233` | Legacy PyInstaller agent jobs — manual opt-in only. |
| `.github/workflows/release.yml:22-287` | Whole legacy `ozma-agent` AppImage/deb/MSI/DMG workflow — manual only. |
| `docker-compose.yml:18` | Labelled `uv pip install ozma-softnode` rollback comment. |
| `Makefile:62-69` | `demo-python` and `legacy-python` escape hatches. |
| `softnode/`, `node/`, `agent/` | Trees untouched; no deletion. |

### G4 — Dev/test references outside this change

These still start, import, or instruct the Python trees. They are unchanged
because they are outside this task, but they must be removed or ported before
Python-tree deletion.

| File:line | Reference |
|---|---|
| `dev/windows-vm/provision.sh:343` | Starts `softnode/soft_node.py`. |
| `dev/riscv-node/connect-to-vms.sh:92,199` | `pgrep -f "soft_node.py..."`. |
| `dev/scripts/build-controller-usb.sh:310-311` | Prints `uv pip install ozma-softnode`. |
| `dev/scripts/build-installer-usb.sh:526-527` | Prints `uv pip install ozma-softnode`. |
| `proxmox-plugin/debian/ozma-display@.service:10` | `ExecStart=/usr/lib/ozma-proxmox/display-service.py %i`. |
| `softnode/install/ozma-evdev.service:13` | `python3 .../evdev_service.py` legacy evdev daemon. |
| `tests/conftest.py:16` | Adds `softnode`, `node`, `agent` to `sys.path`. |
| `tests/unit/softnode`, `tests/unit/agent`, `tests/multiseat`, top-level `tests/test_*` | Import/start the legacy trees; now covered by `python-legacy-tests`. |
| `README.md:113-114` | Still describes `node/` and `softnode/` as Python node daemons. |

### G5 — Release/CI notes

- `docker.yml` and `build.yml` both publish the controller image on tags. Both
  now use `ghcr.io/ozmalabs/ozma-controller`; this is redundant but consistent.
- The aarch64 Rust archive is cross-compiled with `cross` 0.2.5 and the existing
  `Cross.toml` image/pre-build list. It was not built in this session.
- Rust release archives are not signed beyond the published `.sha256`; only the
  OTA controller wheel uses the existing `OTA_SIGNING_KEY` signing step.
- `publish-pypi` is gated on the controller wheel. Legacy softnode/agent wheels
  upload only if their optional job produced them, preserving the rollback path.
