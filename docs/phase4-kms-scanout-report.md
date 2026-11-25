# Phase 4 — KMS scanout proof report

Date: 2026-11-26
Scope: `ozma-multiseat` only (DRM lease plane extension, atomic KMS scanout,
bounded frame-flip loop, tests, and this report). No commit was made;
`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not touched.
`ozma-node`, `ozma-usb`, `ozma-mesh`, and controller Python were not modified.

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
  and waits for each flip completion with a bounded timeout.

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
| `DrmScanoutRequest` | Same selector plus an optional mode, frame count, interval, flip timeout, and a deliberate `force_legacy` baseline flag. |
| `DrmScanoutCommit` | Stable `atomic` / `legacy` marker in status and verification. |
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

### CLI

```text
ozma-multiseat --seat-drm-scanout [SEAT=]CARD:CONNECTOR[@CRTC][:PLANE] \
               [--seat-drm-scanout-mode WIDTHxHEIGHT] \
               [--seat-drm-scanout-frames 1..=300] \
               [--seat-drm-scanout-interval-ms MS] [--json]
```

`--seat-drm-scanout-frames` defaults to `1`; out-of-range values are rejected
before a device is opened. The optional interval paces completed frames, while
flip completion events remain the source of truth for accounting. The proof runs
before display/topology/plan work and exits. `--json` emits the request, final
scanout status (including `plane_id`, `commit`, and `frames`), retained
verification, notes, and typed `frame_error`/`error`/`teardown_error`. A seat
prefix is accepted for CLI symmetry, but the proof is one standalone output.

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

`tests/drm_scanout.rs` (22 tests, was 12 before this phase) now covers:

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
- `flip_unsupported` accounting and frame-stat JSON fields.

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

The `ozma-frame` configfs instance and all plane/CRTC/encoder/connector links
were removed, logind was resumed, and no temporary instance remains. The default
vkms card and the real GPUs were not touched.

## Verification

```text
cargo test -p ozma-multiseat
  218 passed, 0 failed, 15 ignored (includes the 22 fake scanout tests,
  14 lease tests, and the ignored live lease/scanout/frame-loop tests)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 <touched files>
  clean
```

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
