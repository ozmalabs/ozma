# Phase 4 container clean-host gate report

Date: 2026-11-25 (UTC)
Baseline: `08393e79d7b073d2b14c4500f70385ad03863e56` (`docs(phase4): record final live validation at 7b9c0a1`)
Validator: clean-host/Phase 4 subagent

## Verdict

**PASS for the container clean-host gate.** `scripts/clean-host/run.sh
--container --full` passed at `08393e7` in a rootless Podman 6.2.0 container
built from the in-tree `scripts/clean-host/Containerfile`. The image now
contains `libxdo`, so the previously blocked gate is unblocked.

This does **not** make Gate 4 complete. The privileged fresh-host install and a
full Python-free cutover are still outstanding; see
[Remaining issues](#remaining-issues).

## Source and environment

- The primary checkout `/home/matt/ozma/ozma` was dirty with unrelated AIDA64 /
  routing work, so evidence was taken in a clean detached worktree:
  `git worktree add --detach /home/matt/ozma/ozma-container-gate-08393e7 08393e7`.
  `git status --short` was empty before and after both runs.
- No tracked file in that worktree changed during the runs, and the committed
  `Cargo.lock` was **not** rewritten (unlike the `437468a` observation in
  `phase4-cutover-audit.md` §16.1).
- Runtime: rootless Podman 6.2.0 (`podman info` → `Rootless: true`); Docker is
  not installed. No sudo and no host package-manager change was used.
- Disk before the gate: 38 GB free on `/` (931 GB volume, 96 % used). The
  container target grew to 24 GB and was deleted after the run; the host target
  grew to 7.0 GB and was also deleted. See [Cleanup](#cleanup).

## Image build

The cached image was absent (`podman image exists
localhost/ozma-clean-host:1.95` → 1). Two untagged 3.38 GB images from earlier
attempts existed but predated `libxdo`; they were left untouched.

The Containerfile **already** lists `libxdo-dev` (line 27 at `08393e7`), added in
`ce4db163`. The blocker was a stale cached image, not a missing Containerfile
entry, so **no Containerfile change was required**.

```bash
cd /home/matt/ozma/ozma-container-gate-08393e7
timeout 1200 podman build --network=host \
    -t localhost/ozma-clean-host:1.95 \
    -f scripts/clean-host/Containerfile scripts/clean-host
```

| Item | Value |
|---|---|
| Start / end (UTC) | 2026-11-25T03:31:31Z / 2026-11-25T03:31:51Z |
| Result | **PASS** (`Successfully tagged`, exit 0) |
| Image ID | `d84d9288cc78bb5fb7d62ad5e1eb4e694243fcb11515a4e2ece1c7faa1ca2db4` |
| Local manifest digest | `sha256:c39f7af3fa52bc6135576b9fd03ffbdc1af2cdb90b6c348db94ee72ff0571a64` |
| Size | 3,383,150,746 bytes (3.38 GB) |
| Base | `docker.io/library/rust:1.95-bookworm` (`d617f1b5c2a1`) |
| Log | `/tmp/ozma-clean-host-1000/logs/container-build.log` |

`--network=host` was used only to guarantee DNS/apt reachability during the
build. `run.sh` itself calls `podman build` without that flag, so a first build
still needs working container-network DNS (see [Remaining issues](#remaining-issues)).

### Package delta

The rebuild added exactly the X11 input-dev packages needed by `enigo`:

| Package | Version |
|---|---|
| `libxdo3` | `1:3.20160805.1-5` |
| `libxdo-dev` | `1:3.20160805.1-5` |

Verified in the image:

```text
dpkg-query: libxdo-dev 1:3.20160805.1-5, libxdo3 1:3.20160805.1-5
/usr/lib/x86_64-linux-gnu/libxdo.so -> libxdo.so.3
```

Debian does not ship an `xdo.pc`, so `pkg-config --exists xdo` is false; the
harness's `has_libxdo()` fallback glob (`/usr/lib/*/libxdo.so`) reports
`PASS  libxdo: found`. This is the intended detection path, not a skip.

## Container gate (`--container --full`)

```bash
cd /home/matt/ozma/ozma-container-gate-08393e7
OZMA_CLEAN_HOST_TARGET=/home/matt/ozma/ozma-container-gate-target \
    timeout 3300 scripts/clean-host/run.sh --container --full
```

| Item | Value |
|---|---|
| Start / end (UTC) | 2026-11-25T03:32:16Z / 2026-11-25T03:34:44Z |
| Exit / final line | 0 / `RESULT: PASS` |
| Harness summary | **20 passed, 0 failed, 1 skipped** |
| `cargo test --workspace` | **2469 passed, 0 failed, 42 ignored** |
| Test result blocks | 192 (154 test binaries + 38 doc-test suites) |
| `ozma-node --lib` inside workspace | 479 passed, 3 ignored |
| `ozma-softnode` inside workspace | 128 passed |
| `ozma-node` / `ozma-virtual-node` / `ozma-multiseat --help` | PASS |
| Synthetic desktop `/health` | PASS, `ok:true` (port 20004) |
| Logs | `/tmp/ozma-clean-host-1000/logs/container-full.log`, `run.log` |

The single skip is `systemd-analyze is not installed in the test image`; the
host run below covers unit verification. Dependency report in the container was
11 passed / 0 failed / 8 skipped (optional runtime tools not installed in the
image). All required build dependencies, including `libxdo`, passed.

## Host comparison (`--host`)

```bash
cd /home/matt/ozma/ozma-container-gate-08393e7
OZMA_CLEAN_HOST_TARGET=/home/matt/ozma/ozma-container-gate-host-target \
    timeout 2700 scripts/clean-host/run.sh --host
```

| Item | Value |
|---|---|
| Start / end (UTC) | 2026-11-25T03:35:10Z / 2026-11-25T03:36:39Z |
| Exit / final line | 0 / `RESULT: PASS` |
| Harness summary | **22 passed, 0 failed, 0 skipped** |
| Dependency report | 17 passed, 0 failed, 2 skipped (`apparmor_parser`, `shellcheck`) |
| `ozma-node --lib` | 479 passed, 3 ignored |
| `ozma-softnode` | 128 passed |
| `systemd-analyze verify` | PASS (host has systemd 262) |
| Logs | `/tmp/ozma-clean-host-1000/logs/host-run-compare.log`, `host-run.log` |

Host and container results agree. The two extra host passes versus the container
are `systemd-analyze verify` and the corresponding non-skip; both runs exercised
the same binaries, installer dry-runs, and synthetic `/health` smoke.

## Remaining issues

1. **First image build needs network.** The Containerfile runs `apt-get update`
   and installs packages at build time. This run used `--network=host` after
   confirming `deb.debian.org` was reachable. `run.sh` does not pass a network
   flag, so an offline or DNS-broken host still cannot build the image from
   scratch. Remediation: build once on a networked host and keep the image, or
   document/propagate a `--network` option in `run.sh`.
2. **Privileged fresh-host install not run.** The rootless scripted surface is
   green, but the system user/udev/AppArmor/libvirt install still requires sudo
   and was out of scope.
3. **`--locked` not asserted.** The worktree stayed clean, but this run did not
   independently run `cargo metadata --locked` / `cargo test --locked`; the
   stale-lock finding in `phase4-cutover-audit.md` §16.1 remains to be closed by
   a dedicated check.
4. **Image lacks optional verifiers.** `systemd-analyze`, `udevadm`,
   `systemd-tmpfiles`, `qemu`, `virsh`, `pactl`, `apparmor_parser`, and
   `shellcheck` are not in the image, so those checks are `SKIP` there. This is
   by design; the host run covers them.
5. **Cosmetic mode label.** `in-container.sh` always calls
   `check-deps.sh --container`, so `--host` prints `Ozma clean-host dependency
   check (container)`. Detection is still correct; only the wording is wrong.
6. **Pre-existing dangling images.** Two untagged 3.38 GB clean-host images
   (`9a91e138532b`, `40e2f74ae7a4`) from earlier attempts remain in rootless
   Podman storage. They predate `libxdo-dev` and were not created by this run, so
   they were not deleted.

## Cleanup

Removed after evidence collection:

- image/tag `localhost/ozma-clean-host:1.95` (`podman rmi -f`, deleted image and
  both intermediate layers);
- detached worktree `/home/matt/ozma/ozma-container-gate-08393e7`
  (`git worktree remove` deregistered it, then `podman unshare rm -rf` removed
  the leftover `target/` mount point, which rootless Podman had created as
  subuid `100000`);
- container target `/home/matt/ozma/ozma-container-gate-target` (24 GB);
- host target `/home/matt/ozma/ozma-container-gate-host-target` (7.0 GB).

`run.sh` uses `--rm`, so no containers or volumes were left. No repository
commit, tag, product source, `docs/parity-matrix.md`, or
`docs/plan-execution-status.md` was changed. Small text logs were kept at
`/tmp/ozma-clean-host-1000/logs/` as evidence.
