# Phase 4 — KMS scanout proof report

Date: 2026-11-26
Scope: `ozma-multiseat` only (DRM lease plane/writeback extension, atomic KMS
scanout, bounded frame-flip loop, writeback pixel capture, CRC blocker, tests,
and this report). No commit was made;
`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not touched.
This task did not modify `ozma-node`, `ozma-usb`, `ozma-mesh`, or controller
Python; unrelated working-tree edits from another session may be present.

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
3. **No frame loop.** The proof committed one static frame and never consumed
   a page-flip completion event. It could not show buffer rotation, repeated
   flips, damage-style updates, or flip accounting.
4. **No pixel capture.** Verification re-read KMS object state and re-checksummed
   the CPU mapping, but it never proved that the CRTC actually emitted the
   expected pixels. It had no writeback job, no CRC comparison, and no typed
   blocker for a kernel that supports neither.

This phase extends the same opt-in proof:

- `[SEAT=]CARD:CONNECTOR[@CRTC][:PLANE]` leases one validated plane when named;
- scanout uses an atomic `set_plane` + `set_crtc` property commit with a
  page-flip event;
- legacy `set_crtc`/`page_flip` remains the fallback **only** when atomic KMS is
  unsupported, with a typed `legacy_commit_fallback` note;
- verification reports plane, CRTC, and framebuffer state and status exposes
  `commit: "atomic" | "legacy"`;
- `--seat-drm-scanout-frames 2..=300` runs a bounded frame loop over two or
  three rotating dumb buffers, draws a deterministic moving damage rectangle,
  and waits for each flip completion with a bounded timeout;
- `--seat-drm-verify writeback` leases a compatible writeback connector on the
  same CRTC, captures each frame into an XRGB8888 buffer, and compares
  visible-RGB checksums; `crc` records a precise typed blocker where the safe
  DRM API cannot issue CRC ioctls.

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
| `DrmScanoutRequest` | Same selector plus an optional mode, frame count, interval, flip timeout, verification mode, writeback timeout/retries, and a deliberate `force_legacy` baseline flag. |
| `DrmScanoutCommit` | Stable `atomic` / `legacy` marker in status and verification. |
| `DrmScanoutVerifyMode` | Stable `none` / `writeback` / `crc` pixel-verification selector. |
| `DrmScanoutCapture` | Bounded captured-frame count, visible-RGB checksums, expected checksums, mismatches, timeouts, and retries. |
| `DrmScanoutVerificationBlocker` | Typed `kms-verification-unavailable` evidence when a requested mechanism cannot run. |
| `DrmScanoutFramePlan` / `DrmScanoutFrameStats` | Bounded loop parameters and submitted/completed/dropped/missed/flip/interval/checksum accounting. |
| `DrmScanoutBackend` / `DrmScanoutHandle` | Start, status, verify, bounded `run_frames`, teardown, active. |
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

### Bounded frame loop

`start` still performs the initial modeset commit and returns an active handle.
The original one-frame proof is unchanged: `frames` defaults to `1`, one dumb
buffer is allocated, and no completion event is consumed. `frames > 1` switches
to the frame-loop path:

1. `start` allocates `2` buffers for two frames and `3` for three or more,
   filling each with the base pattern and creating one framebuffer per buffer.
2. The initial modeset is frame zero. The loop asks for
   `PAGE_FLIP_EVENT`, so the modeset commit reports its own completion.
3. `run_frames(plan)` waits for frame zero's flip event. Only then is it counted
   as completed. A timeout leaves `completed = 0`, `dropped = 1`,
   `missed = 1`.
4. For each later frame, the next rotating buffer receives only a deterministic
   moving rectangle (`frame_damage_rect`/`apply_frame_damage`, 3x3 position
   cycle, frame-dependent colour). This is damage-style **updating**, not a
   damage tracker.
5. The frame is committed as an atomic `FB_ID`-only page flip (or a legacy
   `page_flip(EVENT)` in the legacy path), then `wait_for_flip` polls the lessee
   fd for `POLLIN` and drains `Event::PageFlip` for the leased CRTC. The wait is
   bounded by `flip_timeout_ms` (default 1000 ms).
6. `DrmScanoutFrameStats` records `requested`, `submitted`, `completed`,
   `dropped`, `missed`, `buffer_count`, `page_flip_events`, `damage_updates`,
   `average_interval_us` (observed completion instants), `last_sequence`, and
   the final re-mapped buffer checksum.
7. A failed commit or timeout is a typed error (`flip_failed`,
   `flip_unsupported`, or `flip_timeout`). Partial stats are stored in
   `DrmScanoutStatus::frames` before returning, and a frame is never counted as
   completed without a flip event.
8. Teardown clears the CRTC, destroys every framebuffer and dumb buffer in the
   rotation, and revokes the lease. A second run on the same objects succeeds.

`run_frames` is also callable explicitly. `plan.frames == 1` is a no-op that
returns the initial single-frame stats, so callers cannot accidentally turn the
proof into an unbounded loop.

### Writeback verification

This kernel's vkms (Linux 7.3.5-2-cachyos, `enable_writeback=Y`) exposes a
writeback connector on the same CRTC as the virtual output. The connector
advertises `WRITEBACK_PIXEL_FORMATS` including `XR24`/`AR24`; `WRITEBACK_FB_ID`,
`CRTC_ID`, and `WRITEBACK_OUT_FENCE_PTR` are present. Writeback is therefore the
strongest available mechanism and is the implemented `writeback` mode.

The CRC path was also checked. vkms implements the kernel CRC callbacks
(`vkms_get_crc_sources`/`vkms_set_crc_source`), but userspace capture requires
`DRM_IOCTL_CRTC_GET_SEQUENCE`, `DRM_IOCTL_CRTC_SET_CRC_SOURCE`, and
`DRM_IOCTL_CRTC_GET_CRC`. The safe `drm = 0.15` surface has no CRC wrapper and
this crate has `#![forbid(unsafe_code)]`, so `crc` records the typed
`kms-verification-unavailable` blocker with kernel evidence. It never claims a
capture it did not perform.

Writeback flow:

1. When `--seat-drm-verify writeback` is requested, resolve the scanout CRTC
   before leasing, discover writeback connectors on the lessor with
   `DRM_CLIENT_CAP_WRITEBACK_CONNECTORS`, and select the lowest-id connector whose
   encoder advertises that CRTC.
2. Add the writeback connector id to the same `CREATE_LEASE` request
   (`DrmLeaseRequest::with_writeback_connector`). The lessor enables `Atomic`
   then `WritebackConnectors`; the connector is validated as `Writeback` and as
   CRTC-compatible before the ioctl. `DrmLeaseHandle::writeback_connector_id`
   reports it.
3. On the lessee, enable `WritebackConnectors` after the scanout atomic commit,
   read the writeback connector properties with the existing 256-property bound,
   and confirm `WRITEBACK_PIXEL_FORMATS` contains `XR24`.
4. Allocate one XRGB8888 capture dumb buffer/framebuffer of the current mode
   size. The capture size is bounded by `SCANOUT_MAX_VERIFY_BYTES` (64 MiB); a
   larger mode records a blocker rather than allocating unbounded memory.
5. Queue a writeback job with `CRTC_ID` and `WRITEBACK_FB_ID` for the capture
   framebuffer, wait for the commit's page-flip event, then wait (bounded) for
   the capture buffer to leave the `0xA5` sentinel. The capture is compared with
   the scanout buffer using `visible_rgb_checksum`: FNV-1a over exactly
   `width * 4` bytes per row, masking the unused alpha byte. Pitch padding and
   vkms's alpha normalisation therefore do not create false mismatches, while
   every visible RGB pixel is covered.
6. Capture after the initial modeset frame and after each completed loop frame;
   `verify()` captures the current frame again. Status reports
   `verification_mode`, `writeback_connector_id`, `capture.captured_frames`,
   per-frame `checksums`/`expected_checksums`, `mismatches`, `timeouts`, and
   `job_retries`. A mismatch is retained and makes `verify` return
   `verification_failed`; a timeout is `writeback_timeout`.
7. Teardown waits for the last capture, clears `WRITEBACK_FB_ID`, detaches the
   writeback connector with `ALLOW_MODESET` (bounded retries absorb the kernel's
   deferred job cleanup), destroys the capture framebuffer/buffer, then clears
   the scanout plane/CRTC, destroys the rotating buffers, and revokes the lease.

Bounds: `writeback_timeout_ms` is validated to `1..=60000` (default 1000),
`writeback_retries` is `0..=3` (default 1 and used only for transient
`EBUSY`/`EAGAIN` commit failures), checksum samples are capped at
`SCANOUT_MAX_CAPTURE_SAMPLES = 300`, and the job queue is serialized by waiting
for each capture before another commit.

Atomic is required. `--seat-drm-verify writeback` with a forced legacy commit, a
legacy fallback, no compatible writeback connector, missing writeback
properties, a non-XRGB format set, or an oversized capture buffer leaves the
scanout proof intact but records a typed blocker; `verify` then returns
`kms-verification-unavailable`.

### CLI

```text
ozma-multiseat --seat-drm-scanout [SEAT=]CARD:CONNECTOR[@CRTC][:PLANE] \
               [--seat-drm-scanout-mode WIDTHxHEIGHT] \
               [--seat-drm-scanout-frames 1..=300] \
               [--seat-drm-scanout-interval-ms MS] \
               [--seat-drm-verify writeback|crc|none] \
               [--seat-drm-verify-timeout-ms MS] \
               [--seat-drm-verify-retries 0..=3] [--json]
```

`--seat-drm-scanout-frames` defaults to `1`; out-of-range values are rejected
before a device is opened. The optional interval paces completed frames, while
flip completion events remain the source of truth for accounting.
`--seat-drm-verify` defaults to `none`; the timeout/retry flags are valid only
with `--seat-drm-scanout` and are range-checked there. The proof runs before
display/topology/plan work and exits. `--json` emits the request, final scanout
status (including `plane_id`, `commit`, `frames`, `verification_mode`,
`verification_blocker`, and `capture`), retained verification, notes, and typed
`frame_error`/`error`/`teardown_error`. A seat prefix is accepted for CLI
symmetry, but the proof is one standalone output.

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
| `InvalidFrameCount` | `invalid_frame_count` |
| `FlipTimeout` | `flip_timeout` |
| `FlipUnsupported` | `flip_unsupported` |
| `FlipFailed` | `flip_failed` |
| `VerificationFailed` | `verification_failed` |
| `VerificationUnavailable` | `kms-verification-unavailable` |
| `WritebackTimeout` | `writeback_timeout` |
| `WritebackFailed` | `writeback_failed` |
| `Released`, `InvalidRequest`, `Io` | matching snake-case names |
| `Lease(DrmLeaseError)` | underlying lease code |

Lease additions include `incompatible_plane` and `incompatible_writeback`, plus
`unknown_object` for a missing plane or writeback connector id.
`DRM_LEASE_PLANE_ID_ENV` (`OZMA_DRM_PLANE_ID`) exports an explicitly leased
plane id alongside the existing fd/connector/CRTC exports. The writeback
connector id is a scanout-verification detail rather than a seat environment
export.

## Tests

Fake-only tests never open `/dev/dri`, allocate a real dumb buffer, or program a
CRTC.

`tests/drm_lease.rs` (14 tests, was 12) adds:

- parsing `CONNECTOR:PLANE` and `CONNECTOR@CRTC:PLANE`, including rejection of
  plane id zero and non-numeric ids;
- fake lease plane compatibility, unknown plane, incompatible CRTC, and reported
  `plane_id`/`plane_ids`.

`tests/drm_scanout.rs` (33 tests, was 12 before this phase) now covers:

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
- deterministic pattern fill with pitch padding;
- one-frame default and typed `invalid_frame_count` bounds;
- deterministic moving damage rectangle and rotating buffer count;
- a four-frame loop: three buffers, four submitted/completed flips, three damage
  updates, buffer reuse, average interval, final checksum, and post-loop
  verification of the current framebuffer;
- a legacy frame loop using `page_flip(EVENT)` for every frame;
- teardown after a loop destroying every framebuffer and buffer before revoke;
- initial flip timeout accounting (`completed = 0`, `dropped = 1`,
  `missed = 1`) and a mid-loop `flip_failed` with partial stats;
- `flip_unsupported` accounting and frame-stat JSON fields;
- writeback success: every loop frame captured, matching visible-RGB checksums,
  `writeback_connector_id`, `writeback_capture` note, and capture teardown events
  before scanout disable;
- writeback checksum mismatch counted while the loop continues, followed by
  `verification_failed` from `verify`;
- writeback capture timeout (`writeback_timeout`, `timeouts=1`) and commit
  failure (`writeback_failed`);
- unavailable writeback and `crc` mode: typed `kms-verification-unavailable`
  blocker with kernel evidence and no fabricated capture;
- forced legacy plus writeback: typed blocker;
- writeback timeout/retry request bounds and mode parsing;
- writeback capture JSON fields (`verification_mode`, `captured_frames`,
  `checksums`, `expected_checksums`, `mismatches`, `timeouts`);
- `visible_rgb_checksum` masking alpha and ignoring pitch padding.

`tests/live_drm_scanout.rs` is `#[ignore]`d and gated by `OZMA_DRM_LIVE=1`. It
refuses any non-vkms card. On a configfs vkms instance it discovers an
XRGB8888 primary plane compatible with the chosen CRTC, leases that plane
explicitly, verifies the atomic plane/CRTC/framebuffer state, re-reads the CRTC
and plane through a second card fd, tears down, checks the proof framebuffer is
no longer attached to either object, and repeats the full cycle. If vkms exposes
no compatible plane, or the explicit plane proof fails, it records the blocker
and runs the legacy `set_crtc` baseline instead.

A second ignored test,
`vkms_scanout_frame_loop_has_real_flip_completions_and_clean_teardown`, runs
10–30 frames (`OZMA_DRM_FRAMES`, default 12) with a 16 ms pacing interval. It
asserts real completion events for every submitted frame, no drops or misses,
three rotating buffers, the damage-update count, an average interval, a final
checksum, verified current-framebuffer state, closed lessee fd after teardown,
and a second successful frame loop on the same connector/CRTC/plane.

`tests/live_drm_scanout.rs` also has two ignored writeback tests:

- `vkms_scanout_writeback_capture_matches_pattern_and_tears_down` discovers the
  writeback connector, leases it with the display connector/CRTC/plane, captures
  three scanout frames plus a `verify` frame, asserts matching checksums and
  zero mismatches/timeouts, tears down, checks the lessee fd is closed, and
  repeats a second capture cycle on the same objects. If no writeback connector
  is compatible it asserts the typed blocker and no capture instead.
- `vkms_scanout_crc_records_precise_typed_blocker` asserts the
  `kms-verification-unavailable` CRC blocker, its kernel evidence, and that no
  capture is invented.

`tests/live_drm_lease.rs` keeps its disconnect/reconnect/revoke cycle and now
constructs the request with the new optional plane and writeback selector
fields.

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

### Live frame loop

A dedicated configfs instance `/sys/kernel/config/vkms/ozma-frame` was created
with one primary plane (`type=1`), one CRTC, one encoder, one connected virtual
connector, and `enabled=1`. `systemd-logind` was frozen for the run so it could
not take DRM master on the new card. The test leased the primary plane and ran
the bounded loop:

```text
$ OZMA_DRM_LIVE=1 OZMA_DRM_CARD=/dev/dri/card4 OZMA_DRM_FRAMES=12 \
    target/debug/deps/live_drm_scanout-... vkms_scanout_frame_loop --ignored --nocapture
vkms frame loop: card=/dev/dri/card4 connector=Virtual-3 crtc=41 plane=Some(36) frames=12
live frame stats: DrmScanoutFrameStats {
    requested: 12,
    submitted: 12,
    completed: 12,
    dropped: 0,
    missed: 0,
    buffer_count: 3,
    page_flip_events: 12,
    damage_updates: 11,
    average_interval_us: Some(33361),
    last_sequence: Some(28),
    final_checksum: Some(1428349092),
}
verification: crtc_id=41 crtc_framebuffer=Some(48) plane_id=Some(36)
  plane_framebuffer=Some(48) pattern_checksum=1428349092 ok=true
second frame loop completed and released
test result: ok. 1 passed; 0 failed; 0 ignored
```

The completion count equals the submitted count because each flip waited for a
real vkms `PAGE_FLIP_EVENT`; `average_interval_us` comes from observed completion
instants, not from the requested interval. The current framebuffer `48` was
verified on both the CRTC and plane, and a second 12-frame loop on the same
objects succeeded, proving that all three dumb buffers/framebuffers and the
lease were released by teardown.

The CLI exercised the same path at the upper bound:

```text
$ target/debug/ozma-multiseat --seat-drm-scanout /dev/dri/card4:Virtual-3 \
      --seat-drm-scanout-frames 30
scanout: /dev/dri/card4:Virtual-3 connector_id=Some(43) crtc=Some(41) \
  plane=Some(36) commit=atomic lessee=Some(1) fd=None
  mode=1024x768 1024x768 pitch=4096 framebuffer=Some(48) buffer=Some(3) ...
  frames: requested=30 submitted=30 completed=30 dropped=0 missed=0 buffers=3 \
    flip_events=30 damage_updates=29 avg_interval_us=Some(16663) \
    last_sequence=Some(1108) final_checksum=Some(436787364)
  verify: ok=true crtc_framebuffer=Some(48) plane_framebuffer=Some(48) ...
  note[frame_loop]: bounded frame loop: 30 frames across 3 rotating dumb buffers, \
    damage pattern moving-damage-rect-xrgb8888, flip timeout 1000ms
  scanout applied=false released=true
```

A 16-frame run with `--seat-drm-scanout-interval-ms 10` also completed all 16
flips (`dropped=0`, `missed=0`, `flip_events=16`), so the optional pacing
interval does not replace event accounting.

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

### Live writeback capture

A fresh configfs vkms instance `/sys/kernel/config/vkms/ozma-live` was created
with one primary plane, one CRTC with `writeback=1`, one encoder, and one
connected virtual connector. To keep the host compositor/logind from taking DRM
master before the proof opened the new test card, the udev exec queue was
stopped for the run; the card node was also mode `000` while root ran the test.
No real GPU or session was touched and the queue was restarted in the cleanup
trap.

Capability evidence from the live run:

```text
vkms writeback evidence: card=/dev/dri/card4 driver=faux_driver \
  connector=Virtual-3 crtc=41 plane=Some(36) \
  writeback=[DrmWritebackConnector { name: "Writeback-3", object_id: 43, \
  possible_crtcs: [41] }]

live writeback capture: DrmScanoutCapture {
  writeback_connector_id: 43, width: 1024, height: 768, pitch: 4096, format: "XR24",
  captured_frames: 3, mismatches: 0, timeouts: 0, job_retries: 0,
  checksums: [3291019389, 463476861, 1910395005],
  expected_checksums: [3291019389, 463476861, 1910395005],
}
live writeback verification: ... verification_mode: Writeback, capture: Some(
  DrmScanoutCapture { captured_frames: 4, mismatches: 0, timeouts: 0,
  checksums: [3291019389, 463476861, 1910395005, 1910395005], ... }), ok: true
test result: ok. 1 passed; 0 failed
```

The four captured frames are the initial modeset frame, the two damage-update
frames, and the extra `verify` capture of the current frame. The expected
checksums came from the scanout buffers themselves; every captured visible-RGB
checksum matched. Teardown cleared/detached writeback, destroyed the capture
framebuffer/buffer, cleared scanout, revoked the lease, and a second full cycle
succeeded.

The CRC test on the same vkms card produced:

```text
crc blocker evidence: DrmScanoutVerificationBlocker {
  kind: "kms-verification-unavailable", mechanism: "crc",
  reason: "CRTC CRC capture is not exposed by the safe DRM API used by ozma-multiseat",
  kernel_evidence: [
    "card /dev/dri/card4 driver faux_driver",
    "kernel 7.3.5-2-cachyos",
    "vkms enable_writeback=Y",
    "vkms create_default_dev=Y",
    "vkms implements CRTC CRC callbacks (vkms_get_crc_sources/vkms_set_crc_source)",
    "userspace CRC needs DRM_IOCTL_CRTC_GET_SEQUENCE, DRM_IOCTL_CRTC_SET_CRC_SOURCE, and DRM_IOCTL_CRTC_GET_CRC",
    "drm 0.15.0 exposes no safe CRC wrapper and this crate forbids unsafe code; writeback capture is the implemented strongest mechanism",
  ],
}
test result: ok. 1 passed; 0 failed
```

## Cleanup

After the runs:

```text
$ sudo find /sys/kernel/config/vkms -maxdepth 1
/sys/kernel/config/vkms
$ ls /sys/class/drm | grep -E '^card[0-9]+$'
card0           # xe
card1           # amdgpu
card2           # hermes-kms
card3           # default vkms, not created by this run
$ ps -o stat,comm -p $(pidof systemd-logind)
Ss systemd-logind
```

The `ozma-frame`/writeback `ozma-live` configfs instances and all
plane/CRTC/encoder/connector links were removed, the udev exec queue was
restarted, and no temporary instance remains. The default vkms card and the real
GPUs were not touched. `fuser`/`lsof` were checked before the writeback run; the
proof accepted only the explicitly named vkms test node.

## Verification

```text
cargo test -p ozma-multiseat
  241 passed, 0 failed (includes the 33 fake scanout tests, 14 lease tests,
  and the ignored live lease/scanout/frame-loop/writeback/CRC tests)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

cargo fmt -p ozma-multiseat
  clean
```

Live writeback and CRC tests were run explicitly with `OZMA_DRM_LIVE=1` against
a temporary configfs vkms card; both passed. The temporary instance was removed
after each run.

## Remaining compositor gaps

- **No compositor or damage tracker.** The frame loop draws one generated
  moving rectangle into a rotating buffer. It does not accept client surfaces,
  composite windows, manage focus, merge dirty regions from clients, or run a
  Wayland/X server. The `scanout_proof_only` note is always present, and the
  damage update is a deterministic proof pattern, not tracked content.
- **No user-space graphics handoff.** A seat child still receives only the
  lease fd/environment. `ozma-node` has no KMS/Wayland compositor, so this is a
  platform proof, not end-to-end seat graphics.
- **One plane, one commit chain.** The proof uses one primary/compatible plane
  and rotates framebuffers on it. It does not exercise overlay/cursor plane
  stacks, format modifiers, per-plane alpha/blend/zpos/color properties, or
  multi-plane atomic composition.
- **Bounded flip loop, not a scheduler.** Frames are presented sequentially with
  an optional fixed interval and a bounded per-flip timeout. There is no
  vblank-driven clock, frame-drop policy, async flip, or latency target; the
  reported average interval is observed completion spacing, and a timeout is a
  typed error rather than a skipped frame.
- **Writeback-only pixel verification.** `writeback` captures the composed CRTC
  into an XRGB8888 buffer and compares visible-RGB checksums. `crc` records a
  typed blocker because the safe DRM surface cannot issue the CRC ioctls and
  this crate forbids unsafe code; it is not a CRC fallback implementation.
- **No writeback out-fence ownership.** The job waits for the commit page-flip
  event and polls the capture buffer for the sentinel to clear. It does not pass
  `WRITEBACK_OUT_FENCE_PTR` or own the returned sync-file fd, because that would
  require raw-fd unsafe code. Commits are serialized and teardown waits, but a
  hostile/slow driver could still make this weaker than a fence-complete proof.
- **Alpha and pitch normalization.** The checksum masks the alpha byte and ignores
  pitch padding. It therefore proves visible RGB pixels, not the exact stored
  bytes or alpha channel. `WRITEBACK_PIXEL_FORMATS` is checked for `XR24`, but
  other advertised formats are not exercised.
- **Still one output/proof.** Capture is one CRTC and one writeback connector at a
  time. There is no multi-output writeback, cloned output, scaled writeback, or
  concurrent capture/composition test.
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
