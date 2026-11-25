# Phase 4 — DRM lease handoff report

Date: 2026-11-25
Scope: `ozma-multiseat` only (source, tests, this report, and
`docs/multiseat-rust.md`). No commit was made; `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were not touched.

## Problem

The multi-seat planner could assign one display output per seat, and the
opt-in `xvfb`/`xephyr` backend could give a seat a private virtual X root, but
there was no kernel-level DRM handoff. A seat could not name a connector/CRTC
set and receive an owned lessee fd. DRM leasing (`DRM_IOCTL_MODE_CREATE_LEASE`)
was the remaining platform gap.

This phase adds a typed, opt-in **lease-only** handoff. It does **not** add a
compositor, KMS scanout ownership, a framebuffer, or per-seat physical display
isolation. Status says so explicitly.

## Lease design

New module: `ozma-multiseat/src/drm_lease.rs`.

| Type | Purpose |
|---|---|
| `DrmCard` / `DrmLeaseConnector` | sysfs card name/path/driver/connectors; after `inspect`, connector/CRTC/plane object ids. |
| `DrmCardSelector` | `AutoVirtual` (`auto`/`vkms`) or explicit `/dev/dri/cardN`/`cardN`. |
| `DrmCrtcSelector` | `Auto` or an explicit object id. |
| `DrmLeaseRequest` | `[SEAT=]CARD:CONNECTOR[@CRTC]`, parsed by `FromStr`. |
| `DrmLeaseBackend` | `enumerate`, `inspect`, `acquire`, `list_lease_ids`. |
| `RealDrmLeaseBackend` | production backend over the maintained `drm = 0.15` crate. |
| `DrmLeaseHandle` | owned lessee fd, lessee/object ids, `validate` (`GET_LEASE`), `list_lease_ids`, `revoke`, `is_active`, `set_inheritable`. |
| `DrmLeaseStatus` / `DrmLeaseError` | flattened status and typed failures. |

### Safety rules

- `AutoVirtual` only selects a sysfs card whose driver is `vkms`. Newer kernels
  bind vkms through the generic `faux_driver` bus, so the device identity
  (`/sys/devices/faux/vkms`) and driver name are both checked. A real GPU is
  selected only when the operator names `/dev/dri/cardN`.
- The connector is resolved from `resource_handles()` plus
  `get_connector().to_string()`, never from a hard-coded object id. An explicit
  CRTC id is checked against the current CRTC handles. Unknown objects are
  `DrmLeaseError::UnknownObject`.
- A same-process duplicate is rejected by an in-process object registry before
  any ioctl. Cross-process duplicates remain kernel errors. No fake lease is
  returned.
- `SET_MASTER` is attempted automatically only for a virtual vkms card. A real
  card usually has a compositor as master; ozma does not steal it. Without
  master/CAP_SYS_ADMIN the error is typed `CapabilityRequired`.
- The kernel ABI is provided by the `drm` crate; this module adds no `unsafe`
  code and no hand-rolled ioctl structures. The crate still has
  `#![forbid(unsafe_code)]`.
- `revoke()` issues `DRM_IOCTL_MODE_REVOKE_LEASE` and closes the lessee fd;
  `Drop` also closes it. `GET_LEASE` validates the live object set.
- `FD_CLOEXEC` is cleared only around the node spawn and restored immediately,
  so the child inherits the lessee fd without leaking it to unrelated spawns.

### Typed errors

| Error | `code()` |
|---|---|
| `NoDrm` | `no_drm` |
| `NoPermission` | `no_permission` |
| `Unsupported` | `unsupported_lease` |
| `ExplicitCardRequired` | `explicit_card_required` |
| `UnknownObject` / `UnknownConnector` | `unknown_object` |
| `AlreadyLeased` | `already_leased` |
| `CapabilityRequired` | `capability_required` |
| `UnknownCard`, `InvalidRequest`, `Io`, `LeaseFailed`, `Released` | matching snake-case names |

## Supervisor integration

`SupervisorOptions::drm_leases: BTreeMap<String, DrmLeaseRequest>` is empty by
default. The CLI adds repeatable
`--seat-drm-lease [SEAT=]CARD:CONNECTOR[@CRTC]`; without prefixes it requires
exactly one value per planned seat (plan order), and with prefixes every value
must name a seat id or index.

Lifecycle:

1. **Start:** private display (if any) → DRM lease acquire → ownership/routing/
   helper/audio → node spawn → readiness. A lease failure releases the display
   and earlier seat resources and never spawns the node.
2. **Child export:** `OZMA_DRM_LEASE=1`, `OZMA_DRM_FD`, `OZMA_DRM_LEASE_ID`,
   `OZMA_DRM_CARD`, `OZMA_DRM_CONNECTOR`, `OZMA_DRM_CONNECTOR_ID`, and
   `OZMA_DRM_CRTC_ID`. No unknown argv is appended (so `ozma-node` is not
   modified).
3. **Restart:** an active lease is reused and its environment re-applied; a
   released lease is re-acquired before the restart spawn.
4. **Stop/remove/rollback/Drop:** child first, then lease revoke, then
   display-dependent resources/display. A revoke failure keeps the handle and
   `drm_lease_applied` for a later retry.
5. **Dry run:** reports `would acquire a DRM lease ... (lease-only; no scanout or
   compositor isolation)` and acquires nothing.

Status (omitted when no lease was requested):

```json
"drm_lease": {
  "card": "/dev/dri/card4",
  "connector": "Virtual-3",
  "lessee_id": 1,
  "connector_id": 43,
  "crtc_id": 41,
  "fd": 12,
  "applied": true,
  "released": false,
  "notes": [{"kind": "lease_only_no_scanout", "message": "..."}]
},
"resources": {"drm_lease_applied": true, "drm_lease_released": false}
```

## Tests

New fake-backed `tests/drm_lease.rs` (12 tests) covers:

- card enumeration/inspection and request parsing;
- connector/CRTC object validation and reported ids;
- lease create, `GET_LEASE` validation, revoke, drop;
- duplicate/already-leased and unknown connector/CRTC;
- no-DRM, no-permission, unsupported-lease, capability-required typed errors;
- auto-virtual never selecting a real card;
- supervisor start/stop fd export and revoke, later-seat rollback, missing
  backend, dry-run no-acquire, and status JSON.

`FakeDrmLeaseBackend` lives in `src/fake.rs`, touches no `/dev/dri`, and records
create/revoke/drop events.

New live test `tests/live_drm_lease.rs` is `#[ignore]`d and gated by
`OZMA_DRM_LIVE=1`. It finds the vkms card, leases a connector/CRTC, verifies the
lessee fd and `GET_LEASE`, spawns a child to confirm the fd survives `exec`,
checks the typed duplicate error, revokes, and re-acquires the same objects to
prove the old lease is gone. It refuses a non-vkms card.

## Live evidence

Host: Linux 7.3.5-2-cachyos, `/dev/dri/card0` = `xe`, `card1` = `amdgpu`,
`card2` = `hermes-kms`. The vkms module was loaded for this run
(`sudo modprobe vkms`).

Attempting `SET_MASTER` on the default vkms card returned `EBUSY` because
`systemd-logind` is DRM master on every `master-of-seat` card
(`/sys/kernel/debug/dri/3/clients`). For the live run a **dedicated configfs
vkms instance** was created (`/sys/kernel/config/vkms/ozma-live`) while logind
was paused for the few seconds needed to create and lease it. This did not
touch a real GPU or mode-set a real output. The instance and its links were
removed afterwards, and logind was resumed.

```text
created configfs vkms ozma-live as /dev/dri/card4
running 1 test
vkms card: card4 path=/dev/dri/card4 driver=faux_driver
connectors: [("Virtual-3", Some(43), None, Connected)]
crtcs: [41]
leased lessee=1 fd=Some(4) connector=Virtual-3 id=43 crtc=41 planes=[]
kernel lease objects: DrmLeaseObjects { connectors: [43], crtcs: [41], planes: [] }
child fd inheritance probe: exit status: 0
lessees before revoke: [1]
duplicate lease error: DRM connector 43 on /dev/dri/card4 is already leased
replacement lessee=1 fd=Some(5)
test vkms_lease_create_validate_duplicate_revoke ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Post-run cleanup check:

```text
$ ls /sys/class/drm/ | grep -E '^card[0-9]+$'
card0
card1
card2
card3          # default vkms card; created by the module load
$ sudo ls /sys/kernel/config/vkms/
# empty
$ ps -o stat,comm -p $(pidof systemd-logind)
Ss systemd-logind
```

`vkms` could not be unloaded afterwards because `systemd-logind`/`kwin_wayland`
hold fds on the default virtual card (`modprobe -r vkms` → “Module vkms is in
use”). It was loaded by this run and remains loaded; no real display or mode
was changed.

## Verification

```text
cargo test -p ozma-multiseat
  175 passed, 0 failed, 11 ignored across lib, bin, and integration tests

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```

## Remaining gaps

- **No compositor or scanout.** The lessee fd is a kernel object handoff. No
  compositor is started, no framebuffer is allocated, no CRTC/plane mode is
  programmed, and no physical output is isolated. The `lease_only_no_scanout`
  note is always attached.
- **`ozma-node` does not consume the fd yet.** It receives `OZMA_DRM_*` but has
  no KMS/Wayland/DRM compositor code, so a lease is prepared for a future
  consumer, not end-to-end seat graphics. `ozma-node` was not modified.
- **Master/capability model.** Automatic `SET_MASTER` is limited to virtual vkms
  cards. Real-GPU leasing requires the caller to already be DRM master (or to
  arrange for the compositor to hand over the output). A real compositor should
  use its own lease flow rather than stealing master.
- **Plane leasing.** The request leases connector+CRTC. The kernel report
  currently returns no plane ids for the vkms test. A compositor that needs an
  explicit primary/overlay plane will need a plane selector in
  `DrmLeaseRequest`.
- **Input into a leased seat.** `ozma-seat-input`/uinput isolation is unchanged;
  there is no integration of those devices with a leased compositor session.
- **Controller mode.** The controller still groups seats read-only; there is no
  leased-display lifecycle or capability flag beyond the node status/document.
- **Host logind.** A default vkms card is often already mastered by
  `systemd-logind`; operators need a dedicated/claimed card or master access.
  The typed `capability_required` status makes that visible instead of faking a
  lease.
