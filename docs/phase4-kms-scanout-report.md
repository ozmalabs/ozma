# Phase 4 — KMS scanout proof report

Date: 2026-11-26
Scope: `ozma-multiseat` only (DRM lease plane extension, atomic KMS scanout,
tests, and this report). No commit was made; `docs/parity-matrix.md` and
`docs/plan-execution-status.md` were not touched. `ozma-node`, `ozma-usb`,
`ozma-mesh`, and controller Python were not modified.

## Problem

The first scanout proof acquired a connector+CRTC lease, programmed the CRTC
with legacy `set_crtc`, and queued a legacy `page_flip`. That proved the leased
objects could drive pixels, but it did not exercise the two APIs a compositor
actually needs:

1. **Plane leasing.** A lease named only a connector and CRTC. The kernel
   reported the planes it associated with that CRTC, but the proof could not
   request, validate, or report a specific plane.
2. **Atomic commits.** The proof used legacy CRTC programming. It did not build
   an `ATOMIC` request, set connector/CRTC/plane properties, or use an atomic
   page-flip commit, and it had no typed fallback for drivers without atomic
   KMS.

This phase extends the same opt-in proof:

- `[SEAT=]CARD:CONNECTOR[@CRTC][:PLANE]` leases one validated plane when named;
- scanout uses an atomic `set_plane` + `set_crtc` property commit with a
  page-flip event;
- legacy `set_crtc`/`page_flip` remains the fallback **only** when atomic KMS is
  unsupported, with a typed `legacy_commit_fallback` note;
- verification reports plane, CRTC, and framebuffer state and status exposes
  `commit: "atomic" | "legacy"`.

It is still deliberately **not** a compositor, window manager, client renderer,
or host display isolation mechanism. `DrmScanoutStatus` always carries the
`scanout_proof_only` note.

## Design

Module: `ozma-multiseat/src/drm_scanout.rs`, with the lease extension in
`ozma-multiseat/src/drm_lease.rs`.

| Type | Purpose |
|---|---|
| `DrmLeaseRequest` / `DrmPlaneSelector` | Adds an optional explicit plane to the existing card/connector/CRTC selector. |
| `DrmLeaseHandle::plane_id` | Reports the explicitly requested plane separately from the kernel-reported `plane_ids`. |
| `DrmScanoutRequest` | Same selector plus an optional mode and a deliberate `force_legacy` baseline flag. |
| `DrmScanoutCommit` | Stable `atomic` / `legacy` marker in status and verification. |
| `DrmScanoutBackend` / `DrmScanoutHandle` | Start, status, verify, teardown, active. |
| `RealDrmScanoutBackend` | Production path over `RealDrmLeaseBackend` and `drm = 0.15`. |
| `DrmScanoutStatus` / `DrmScanoutVerification` | Card/connector/CRTC/plane/framebuffer/buffer ids, commit kind, pattern checksum, page-flip flag, typed notes/errors. |
| `DrmScanoutError` | Typed atomic, plane, property, format, and verification failures with stable `code()` values. |
| `FakeDrmScanoutBackend` | In-memory lease/plane/buffer/atomic-commit/fallback/teardown model. |

### Plane leasing

The CLI selector is now:

```text
[SEAT=]CARD:CONNECTOR[@CRTC][:PLANE]
```

Examples:

```text
auto:Virtual-1
auto:Virtual-1@41
auto:Virtual-1@41:36
/dev/dri/card4:Virtual-3:36
```

An explicit plane is validated **before** the lease ioctl:

- `GET_PLANE` must succeed on the lessor card;
- `plane::Info::possible_crtcs()` must include the selected CRTC;
- the plane is added to `CREATE_LEASE` and its id is reported in
  `DrmLeaseStatus::plane_id` and `DrmScanoutStatus::plane_id`;
- a same-process duplicate plane is rejected through the existing lease
  registry;
- an unknown plane is `unknown_object`; an incompatible plane/CRTC pair is
  `incompatible_plane`.

When no plane is named, the old connector+CRTC lease shape is kept and the
kernel continues to associate the CRTC's planes with the lease.

One kernel detail matters for the explicit-plane path. When the lessor has not
enabled universal planes, the kernel automatically adds a CRTC's primary and
cursor planes while inserting the lease objects. Naming that same primary plane
explicitly then inserts the id twice; the kernel object idr returns `ENOSPC`.
The lease backend therefore enables `DRM_CLIENT_CAP_UNIVERSAL_PLANES` on the
lessor only when a plane is explicitly requested. With universal planes, the
kernel requires at least one plane in the lease and no longer auto-adds the
CRTC planes, so the explicit request succeeds. With no explicit plane the
legacy capability state is left untouched.

### Atomic scanout flow

1. Resolve and validate the vkms/real card exactly as before.
2. Acquire the lease. On a vkms card the lease backend takes DRM master; on a
   real card the operator must already be master.
3. Use the lessee fd for every KMS operation.
4. Read the leased connector, choose the requested or first advertised mode,
   and create the XRGB8888 dumb buffer/framebuffer.
5. Enable `UniversalPlanes` and `Atomic` client capabilities on the lessee.
   An unsupported atomic capability is the **only** condition that triggers the
   legacy fallback. A commit failure after atomic was enabled stays a typed
   `mode_failed` error.
6. Select a plane:
   - the explicit lease plane when one was named, or
   - a compatible plane from `GET_LEASE` (falling back to
     `GETPLANE_RESOURCES`), preferring `type=Primary`, then overlay/other.
   CRTC compatibility and XRGB8888 support are checked before any property is
   written. Missing plane is `unsupported_plane`; wrong format is
   `invalid_format`.
7. Enumerate connector, CRTC, and plane properties with a fixed
   `MAX_ATOMIC_PROPERTIES = 256` bound. More properties is
   `property_limit_exceeded`; a missing required property is
   `missing_atomic_property`.
8. Create a mode blob and build one atomic request containing:
   - connector `CRTC_ID`;
   - CRTC `ACTIVE`, `MODE_ID`;
   - plane `FB_ID`, `CRTC_ID`, `SRC_X/Y/W/H`, `CRTC_X/Y/W/H`.
   Commit with `ALLOW_MODESET | PAGE_FLIP_EVENT`. The mode blob is destroyed
   after the commit.
9. Verify:
   - `GET_CRTC`: mode, attached framebuffer, position;
   - connector → encoder → CRTC binding;
   - `GET_PLANE`: plane CRTC and framebuffer;
   - `GETFB`: framebuffer size, pitch, bpp, depth, backing handle;
   - dumb buffer size/pitch and a fresh re-map checksum of the scanned-out
     pattern.
10. Teardown:
    - atomic path: one `ALLOW_MODESET` commit clearing connector `CRTC_ID`,
      CRTC `ACTIVE`/`MODE_ID`, and plane `FB_ID`/`CRTC_ID`, then destroy
      framebuffer and dumb buffer, then revoke the lease;
    - legacy path: `set_crtc(None)` then the same destruction/revoke order;
    - the lease is revoked even if a clear/destroy step failed, so the lessee
      fd cannot leak; a second teardown is a no-op.

Legacy fallback still queues `set_crtc` and `page_flip`, and records
`legacy_commit_fallback`. A deliberately forced legacy baseline records
`legacy_commit_forced`. A rejected page flip remains non-fatal and records
`page_flip_unavailable`.

### CLI

```text
ozma-multiseat --seat-drm-scanout [SEAT=]CARD:CONNECTOR[@CRTC][:PLANE] \
               [--seat-drm-scanout-mode WIDTHxHEIGHT] [--json]
```

The proof runs before display/topology/plan work and exits. `--json` emits the
request, final scanout status (including `plane_id` and `commit`), retained
verification, notes, and typed `error`/`teardown_error`. A seat prefix is
accepted for CLI symmetry, but the proof is one standalone output.

### Typed errors

| Error | `code()` |
|---|---|
| `NoDrm` | `no_drm` |
| `NoPermission` | `no_permission` |
| `CapabilityRequired` | `capability_required` |
| `UnsupportedVkms` | `unsupported_vkms` |
| `RealCardRefused` | `real_card_refused` |
| `UnsupportedConnector` | `unsupported_connector` |
| `AtomicUnsupported` | `atomic_unsupported` (fallback trigger) |
| `UnsupportedPlane` | `unsupported_plane` |
| `InvalidFormat` | `invalid_format` |
| `MissingAtomicProperty` | `missing_atomic_property` |
| `PropertyLimitExceeded` | `property_limit_exceeded` |
| `ModeFailed` | `mode_failed` |
| `BufferFailed` | `buffer_failed` |
| `FramebufferFailed` | `framebuffer_failed` |
| `VerificationFailed` | `verification_failed` |
| `Released`, `InvalidRequest`, `Io` | matching snake-case names |
| `Lease(DrmLeaseError)` | underlying lease code |

Lease additions include `incompatible_plane`, plus `unknown_object` for a
missing plane id. `DRM_LEASE_PLANE_ID_ENV` (`OZMA_DRM_PLANE_ID`) exports an
explicitly leased plane id alongside the existing fd/connector/CRTC exports.

## Tests

Fake-only tests never open `/dev/dri`, allocate a real dumb buffer, or program a
CRTC.

`tests/drm_lease.rs` (14 tests, was 12) adds:

- parsing `CONNECTOR:PLANE` and `CONNECTOR@CRTC:PLANE`, including rejection of
  plane id zero and non-numeric ids;
- fake lease plane compatibility, unknown plane, incompatible CRTC, and reported
  `plane_id`/`plane_ids`.

`tests/drm_scanout.rs` (15 tests, was 12) now covers:

- request parsing and explicit mode;
- auto selection refusing a real card while an explicit path is allowed;
- atomic event order: property writes for connector/CRTC/plane, `atomic_commit`,
  `atomic_page_flip`;
- plane verification (`plane_id`, `plane_crtc`, `plane_framebuffer`);
- explicit plane validation and use;
- `atomic_unsupported` fallback with `legacy_commit_fallback` and legacy event
  order;
- forced legacy baseline with `legacy_commit_forced`;
- atomic teardown order (`atomic_disable`, destroy framebuffer, destroy buffer,
  revoke), idempotence, and drop-driven teardown;
- explicit/unknown mode selection;
- all injected typed failures and stable `code()` values;
- verification failure retained in status;
- non-fatal page-flip unavailability;
- status JSON with plane id, `commit:"atomic"`, notes, verification, and
  `released`;
- deterministic pattern fill with pitch padding.

`tests/live_drm_scanout.rs` is `#[ignore]`d and gated by `OZMA_DRM_LIVE=1`. It
refuses any non-vkms card. On a configfs vkms instance it discovers an
XRGB8888 primary plane compatible with the chosen CRTC, leases that plane
explicitly, verifies the atomic plane/CRTC/framebuffer state, re-reads the CRTC
and plane through a second card fd, tears down, checks the proof framebuffer is
no longer attached to either object, and repeats the full cycle. If vkms exposes
no compatible plane, or the explicit plane proof fails, it records the blocker
and runs the legacy `set_crtc` baseline instead.

`tests/live_drm_lease.rs` keeps its disconnect/reconnect/revoke cycle and now
constructs the request with the new optional plane selector.

## Live evidence

Host: Linux 7.3.5-2-cachyos. `/dev/dri/card0` = `xe`, `card1` = `amdgpu`,
`card2` = `hermes-kms`, `card3` = the default vkms instance
(`faux_driver`). A dedicated configfs instance
`/sys/kernel/config/vkms/ozma-atomic` was created with one primary plane, one
CRTC, one encoder, one connected virtual connector, and `enabled=1`.

`systemd-logind` was frozen (`SIGSTOP`) before the instance was created and kept
frozen through the run, so it could not take DRM master on the new card. The
instance was removed and logind resumed after each run. No real GPU, connector,
compositor, or user session was mode-set or stopped.

### Live atomic test with an explicit plane

```text
$ OZMA_DRM_LIVE=1 OZMA_DRM_CARD=/dev/dri/card4 \
    target/debug/deps/live_drm_scanout-... --ignored --nocapture
vkms card: card4 path=/dev/dri/card4 driver=faux_driver
connectors: [("Virtual-3", Some(43), None, Connected, ["1024x768", ...])]
crtcs: [41]
requested crtc: 41 discovered plane: Some(36)
using explicit lease plane 36
scanout status:
  connector_id=Some(43) crtc_id=Some(41) plane_id=Some(36)
  commit=Atomic lessee=Some(1) fd=Some(4)
  mode="1024x768" 1024x768 pitch=4096 framebuffer=46 buffer=1
  pattern="quadrants-border-diagonal-xrgb8888"
  pattern_checksum=3291019389 page_flipped=true applied=true
  notes=[scanout_proof_only, lessee_fd, plane_leased]
verification:
  crtc_id=41 crtc_mode="1024x768" crtc_framebuffer=46 position=(0,0)
  connector_state=connected connector_crtc=41
  plane_id=36 plane_crtc=41 plane_framebuffer=46
  framebuffer_id=46 size=(1024,768) pitch=4096 bpp=32 depth=24
  framebuffer_buffer_handle=2 (GETFB temp handle)
  buffer_size=(1024,768) buffer_pitch=4096 buffer_handle=1
  pattern_checksum=3291019389 ok=true
independent GET_CRTC: crtc 41 framebuffer=Some(46) mode=Some("1024x768")
released status: applied=false released=true lessee_fd=None
post-teardown GET_CRTC: crtc 41 framebuffer=Some(44) mode=Some("1024x768")
post-teardown GET_PLANE: plane 36 crtc=Some(41) framebuffer=Some(44)
second scanout cycle completed and released
test result: ok. 1 passed; 0 failed; 0 ignored
```

The post-teardown `framebuffer=Some(44)` is the kernel fbdev console's own
framebuffer being restored after the lease/master was released. The proof
framebuffer `46` is no longer attached to the CRTC or the plane. A second full
start/verify/teardown cycle succeeded on the same connector/CRTC/plane, proving
the lease, plane, framebuffer, and dumb buffer were released.

During development the first explicit-plane attempt returned `ENOSPC` from
`CREATE_LEASE`. A legacy fallback cycle still completed and verified, which
isolated the cause to the kernel auto-adding the CRTC primary plane while the
explicit request named that same plane. Enabling `UniversalPlanes` on the lessor
for explicit-plane requests removed the duplicate insertion and produced the
atomic run above. The fake test suite keeps both the fallback and forced-legacy
paths covered without a device.

### CLI proof (automatic plane, then explicit plane)

```text
$ target/debug/ozma-multiseat --json --seat-drm-scanout /dev/dri/card4:Virtual-3
auto scanout ids: connector_id=43 crtc_id=41 plane_id=36 commit=atomic
  mode=1024x768 framebuffer=45 pattern_checksum=3291019389
  page_flipped=True applied=False released=True
auto notes: scanout_proof_only, lessee_fd, plane_leased

$ target/debug/ozma-multiseat --json --seat-drm-scanout \
    /dev/dri/card4:Virtual-3@41:36
explicit scanout ids: connector_id=43 crtc_id=41 plane_id=36 commit=atomic
  mode=1024x768 framebuffer=46 pattern_checksum=3291019389
  page_flipped=True applied=False released=True
explicit notes: scanout_proof_only, lessee_fd, plane_leased
explicit verification: ok=True crtc_id=41 crtc_framebuffer=46 plane_id=36
  plane_crtc=41 plane_framebuffer=46 pattern_checksum=3291019389
```

The CLI tears the proof down before printing, so `applied=false` and
`released=true` are expected in the final status; the retained verification is
from while it was active. The automatic run proves a plane is selected from the
lease even when the selector omits one; the explicit run proves a named plane is
validated, leased, committed, verified, and reported.

## Cleanup

After the runs:

```text
$ sudo find /sys/kernel/config/vkms -maxdepth 1
/sys/kernel/config/vkms
$ ls /sys/class/drm | grep -E '^card[0-9]+$'
card0
card1
card2
card3          # default vkms, not created by this run
$ ps -o stat,comm -p $(pidof systemd-logind)
Ss systemd-logind
```

The configfs instance and all plane/CRTC/encoder/connector links were removed,
logind was resumed, and no temporary instance remains. The default vkms card and
the real GPUs were not touched.

## Verification

```text
cargo test -p ozma-multiseat
  211 passed, 0 failed, 14 ignored (includes the 15 fake scanout tests,
  14 lease tests, and the ignored live lease/scanout tests)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 <touched files>
  clean
```

## Remaining compositor gaps

- **No compositor.** The proof draws one generated static framebuffer. It does
  not accept client surfaces, composite windows, manage focus, perform damage
  tracking, or run a Wayland/X server. The `scanout_proof_only` note is always
  present.
- **No user-space graphics handoff.** A seat child still receives only the
  lease fd/environment. `ozma-node` has no KMS/Wayland compositor, so this is a
  platform proof, not end-to-end seat graphics.
- **One plane, one commit.** The proof uses one primary/compatible plane and one
  static framebuffer. It does not exercise overlay/cursor plane stacks, format
  modifiers, per-plane alpha/blend/zpos/color properties, or multi-plane atomic
  composition.
- **No frame event loop.** A `PAGE_FLIP_EVENT` is requested and the commit is
  verified synchronously, but the proof does not consume vblank/flip events,
  schedule frames, or do damage-driven repaint.
- **No capture of the pattern.** vkms writeback output was not wired into this
  proof. Verification is KMS state (CRTC/plane/connector/framebuffer/buffer ids,
  mode, size, pitch) plus a re-mapped checksum of the buffer bytes, not a
  read-back image or CRC.
- **Live scope is vkms.** Atomic plane leasing/commit is validated live only on
  a configfs vkms instance. An explicit real `/dev/dri/cardN` is still supported
  but requires the operator to already be DRM master; automatic real-card
  atomic scanout is not attempted or proven here.
- **No input or session integration.** `ozma-seat-input`/uinput isolation and
  the per-seat display backends are unchanged.
- **Host logind.** A default vkms card is often mastered by `systemd-logind`.
  The live run used a dedicated configfs instance with logind briefly frozen; the
  tool itself reports `capability_required` rather than stealing a real
  session's master.
- **Real-card policy.** An explicit `/dev/dri/cardN` may be used, but the
  operator must already be DRM master. Status records `real_gpu_named`; no
  automatic real-card scanout exists.
