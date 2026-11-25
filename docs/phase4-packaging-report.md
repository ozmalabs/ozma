# Phase 4 packaging report — multiseat and meeting detection

Date: 2026-11-25
Scope: close the `install-rust.sh` packaging gap recorded in
[`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16.3 item 7.

## Problem

`scripts/install-rust.sh` installed only `ozma-node` and `ozma-virtual-node`.
`ozma-multiseat` and `ozma-meeting-detect` are in the workspace and had passing
tests, but a release install produced neither binary, no unit for the seat
supervisor, and no env example. `scripts/install-system.sh` rendered the node
units but not a seat unit, so a host staged before the binaries existed also had
no seat unit to enable later.

## Installed assets

`scripts/install-rust.sh` now installs the optional assets **when the binary is
present** in the target directory (or built by `--build`). Bin names are read
from the crate manifests, not hardcoded.

| Asset | User layout (`~/.local`) | System layout (`--system --prefix /usr/local`) |
|---|---|---|
| multiseat binary | `~/.local/bin/ozma-multiseat` | `/usr/local/bin/ozma-multiseat` |
| meeting binary | `~/.local/bin/ozma-meeting-detect` | `/usr/local/bin/ozma-meeting-detect` |
| multiseat unit | `~/.local/lib/systemd/user/ozma-multiseat.service` | `/usr/local/lib/systemd/system/ozma-multiseat.service` |
| multiseat env example | `~/.local/etc/ozma/ozma-multiseat.env.example` | `/etc/ozma/ozma-multiseat.env.example` |

Name resolution:

- `ozma-multiseat/Cargo.toml` has no `[[bin]]`, so the script falls back to the
  package name (`ozma-multiseat`).
- `ozma-softnode/Cargo.toml` declares two bins; the meeting bin is selected as
  the `[[bin]]` whose name or path contains `meeting`
  (`ozma-meeting-detect`). The historical name is used only if that manifest
  cannot be read.

`--build` builds `ozma-node`, `ozma-softnode` (both bins, so
`ozma-meeting-detect` is included), and `ozma-multiseat`. `scripts/install-system.sh`
renders the same multiseat unit and env example (it does not install binaries)
and writes the enable/rollback commands under the `desktop` profile.

## Unit design

`scripts/systemd/ozma-multiseat.service`:

- `ExecStart=@BIN_DIR@/ozma-multiseat --node-bin @BIN_DIR@/ozma-node
  --seats-file @STATE_DIR@/seats.json --hotplug $OZMA_MULTISEAT_ARGS`
  — the persisted store is the same `seats.json` a seat node can read with
  `--multiseat --seats-file`.
- `--print-plan` and `--dry-run` are **never** used: both print and exit, which
  would make systemd restart-loop the unit instead of supervising seats.
- `Restart=on-failure`, `RestartSec=5s`, `StartLimitIntervalSec=300`,
  `StartLimitBurst=5`, `TimeoutStopSec=30s`, `KillMode=mixed`. The supervisor
  receives SIGTERM and stops/reaps seat children; leftovers are killed after the
  timeout. Per-child restart backoff stays in the CLI.
- `User=@OZMA_USER@`, `Group=@OZMA_GROUP@`,
  `SupplementaryGroups=video input render` — the same account/group rendering
  convention as the node units; the user rendering strips system-only
  directives and retargets `WantedBy=default.target`.
- `PrivateTmp=no` (X11 sockets live in `/tmp/.X11-unix`), with the same
  `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, device
  allow-list, and restricted address families as the desktop node.
- `DISPLAY`/`XAUTHORITY` come from `ozma-multiseat.env`. A system unit cannot
  read a user's `~/.Xauthority` under `ProtectHome=yes`; the example documents
  both the user-unit path and a readable host cookie path.

`scripts/systemd/env/ozma-multiseat.env.example` documents `DISPLAY`, optional
`XAUTHORITY`, one-line `OZMA_MULTISEAT_ARGS` examples for the opt-in
ownership/routing/audio/private-display/input-isolation modes, and `RUST_LOG`.

### Meeting detection: no unit

`ozma-meeting-detect` has no documented service mode. Its only entry point is a
foreground IPC server bound to `$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`
(fallback `/tmp/ozma-meeting-detect.sock`), with no socket-path flag, no
daemon/system mode, and no controller report wiring. Adding a system unit would
put the socket somewhere clients cannot predict and would conflict with
`PrivateTmp`. It is therefore installed as an on-demand binary only:

```bash
ozma-meeting-detect --interval 5 --ics-path ~/calendar.ics
```

Run it in the session that owns the calendar data. This is documented in
`scripts/systemd/README.md` and
[`multiseat-rust.md`](multiseat-rust.md#meeting-detection-packaging).

## Opt-outs

| Flag | Effect |
|---|---|
| `--no-multiseat` | Skips the `ozma-multiseat` binary, `ozma-multiseat.service`, and `ozma-multiseat.env.example`. `--build` also stops building the package. |
| `--no-meeting-detect` | Skips the `ozma-meeting-detect` binary. |
| `--no-helper` / `--no-env` | Unchanged existing opt-outs and still compose. |

A missing optional binary is a note, not a fatal error: the node install still
succeeds, and the dependent unit/env is skipped so a dangling `ExecStart` is not
installed. `--dry-run` prints every planned `install`/`write`/`copy`/`mkdir` and
writes nothing.

## Verification evidence

All commands ran in `/home/matt/ozma/ozma` without sudo or a live systemd.

```bash
bash -n scripts/install-rust.sh scripts/install-system.sh \
        scripts/systemd/verify-units.sh scripts/verify-system-install.sh \
        scripts/clean-host/in-container.sh scripts/clean-host/run.sh
# all OK

scripts/install-rust.sh --dry-run \
    --from-target /tmp/opencode/ozma-pkg-stage/bin \
    --prefix /tmp/opencode/ozma-pkg-root
# exit 0; DRY install ozma-multiseat + ozma-meeting-detect;
# DRY write .../ozma-multiseat.service; DRY copy .../ozma-multiseat.env.example;
# no files created

scripts/install-rust.sh --build --dry-run --prefix /tmp/opencode/ozma-pkg-build
# exit 0 without prebuilt binaries: prints the cargo build (including
# -p ozma-multiseat), the four binary installs, the multiseat unit/env writes,
# and writes nothing

scripts/install-rust.sh --dry-run --no-multiseat --no-meeting-detect ...
# multiseat: skipped; meeting: skipped; no multiseat unit/env planned;
# no files created

scripts/install-rust.sh --from-target /tmp/opencode/ozma-pkg-stage/bin \
    --prefix /tmp/opencode/ozma-pkg-user
# exit 0 (non-root user layout); installed 4 binaries, 4 user units,
# 5 env examples; user unit has no User=/Group=/DeviceAllow=/ReadWritePaths=
# and ends WantedBy=default.target

scripts/systemd/verify-units.sh
# PASS system + user for ozma-node@, ozma-node-vm@, ozma-virtual-node,
# and ozma-multiseat (8/8)

scripts/verify-system-install.sh
# 82 passed, 0 failed, 1 skipped (apparmor_parser absent)
# RESULT: PASS
```

`verify-system-install.sh` now also asserts:

- the multiseat unit exists in the rendered system tree;
- `ExecStart` uses `<root>/var/lib/ozma-node/seats.json`;
- `ExecStart` contains neither `--print-plan` nor `--dry-run`;
- `User=`/`Group=` and `Restart=on-failure` are rendered;
- the env example documents `DISPLAY` and `OZMA_MULTISEAT_ARGS`;
- the unit mode is `0644`;
- the operator-commands file contains both the multiseat enable and rollback
  lines;
- 5 env examples render (was 4).

`scripts/clean-host/in-container.sh` builds `ozma-multiseat`, requires all four
binaries, adds `ozma-multiseat --help` to the smoke, and checks both the normal
and opted-out installer dry runs for the new assets. It does **not** invoke
`ozma-meeting-detect --help`: that binary parses no `--help` flag and would run
its server.

## Remaining privileged / out-of-scope gaps

- No privileged fresh-host install was run: creating the `ozma-node` account,
  installing units under `/usr/local`, reloading systemd, and enabling
  `ozma-multiseat.service` remain operator steps. The exact commands are in
  `/etc/ozma/install-operator-commands.txt`.
- A system `ozma-multiseat.service` still needs a reachable X display and cookie
  (`DISPLAY`, `XAUTHORITY`) or a `systemctl --user` unit for a live session.
  This is documented, not automated: choosing a session cookie is host policy.
- `ozma-meeting-detect` remains unwired to the controller report path (audit
  §16.3 item 5) and has no unit by design. Closing that needs a product change,
  which this packaging pass deliberately avoids.
- No product Rust code or controller Python was changed; the one-release Python
  rollback path (`OZMA_DEMO_BACKEND=python`, `demo/run_demo.sh --python`, and the
  Python trees) is untouched.
- `docs/parity-matrix.md` and `docs/plan-execution-status.md` were not edited,
  per instructions.

## Files changed

- `scripts/install-rust.sh` — manifest-driven optional binaries, opt-outs,
  conditional unit/env install, build package list, next-step text.
- `scripts/install-system.sh` — renders `ozma-multiseat.service`, operator
  enable/rollback lines.
- `scripts/systemd/ozma-multiseat.service` — new unit.
- `scripts/systemd/env/ozma-multiseat.env.example` — new env example.
- `scripts/systemd/verify-units.sh` — stubs and coverage for the new unit.
- `scripts/verify-system-install.sh` — file/env counts, unit content/mode,
  operator commands, custom-account coverage.
- `scripts/clean-host/in-container.sh` — build/help/dry-run/opt-out checks.
- `scripts/systemd/README.md`, `docs/clean-host.md`, `docs/multiseat-rust.md` —
  packaging and unit documentation.
- `docs/phase4-packaging-report.md` — this report.

Temporary roots under `/tmp/opencode/ozma-pkg-*` and the staged systemd renders
were removed after the checks; no host unit was installed, enabled, or started.
