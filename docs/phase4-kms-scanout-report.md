# Phase 4 — KMS scanout proof report

Date: 2026-11-26
Scope: `ozma-multiseat` only (new module, CLI proof flag, tests, and this
report plus a follow-on note in `docs/phase4-drm-lease-report.md`). No commit
was made; `docs/parity-matrix.md` and `docs/plan-execution-status.md` were not
touched. `ozma-node`, `ozma-usb`, `ozma-mesh`, and controller Python were not
modified.

## Problem

The DRM lease handoff gave a seat an owned lessee fd for one connector/CRTC,
but acquiring a lease does not prove the objects can actually be used for
scanout. There was no dumb buffer, framebuffer, mode program, page flip, or
post-modeset verification. A lease-only report could therefore say “objects
handed off” but not “pixels scanned out”.

This phase adds an opt-in **scanout proof**: one static generated pattern is
programmed on one leased vkms connector/CRTC, verified through KMS, and torn
down. It is deliberately **not** a compositor, window manager, client renderer,
or host display isolation mechanism.
`DrmScanoutStatus` always carries the `scanout_proof_only` note.

## Design

New module: `ozma-multiseat/src/drm_scanout.rs`.

| Type | Purpose |
|---|---|
| `DrmScanoutRequest` | Same `[SEAT=]CARD:CONNECTOR[@CRTC]` selector as `DrmLeaseRequest`, plus an optional exact mode name. |
| `DrmScanoutBackend` | `start(&request) -> owned handle`. |
| `DrmScanoutHandle` | `status()`, `verify()`, `teardown()`, `is_active()`. |
| `RealDrmScanoutBackend` | Production path over `RealDrmLeaseBackend` and the `drm = 0.15` crate. |
| `DrmScanoutStatus` / `DrmScanoutVerification` | Card/connector/CRTC/mode/framebuffer/buffer ids, pattern checksum, page-flip flag, proof-only notes, typed error. |
| `DrmScanoutError` | Typed failures with stable `code()` values. |
| `FakeDrmScanoutBackend` in `fake.rs` | In-memory buffer/mode/flip/teardown model and injected failures. |

### Flow

1. Resolve the card. `AutoVirtual` selects only a sysfs `vkms`/`faux_driver`
   card. A real GPU is allowed only when the operator names its
   `/dev/dri/cardN` explicitly (`ensure_scanout_card`).
2. Acquire the existing connector/CRTC lease. On a vkms card the lease backend
   takes DRM master; on a real card the operator must already be master.
3. Use the **lessee fd**, never the lessor master fd, for KMS operations.
   `DrmLeaseHandle::as_fd()` supplies a safe borrowed fd; the module has no
   `unsafe` code.
4. Read the leased connector, choose the requested mode or the first advertised
   (normally preferred) mode, and reject a disconnected connector or empty mode
   list with `unsupported_connector`.
5. Create a `XR24`/XRGB8888 dumb buffer at `SCANOUT_BPP = 32`, depth 24, map it,
   fill `quadrants-border-diagonal-xrgb8888`, and compute an FNV-1a checksum over
   the mapping. Rows respect the buffer pitch.
6. Add a framebuffer, program `set_crtc(CRTC, fb, (0,0), [connector], mode)`,
   then queue one `page_flip`. A rejected flip is non-fatal and recorded as
   `page_flip_unavailable`; the `set_crtc` commit still programmed the mode.
7. Verify through KMS:
   - `GET_CRTC`: CRTC mode name, attached framebuffer id, position;
   - connector → encoder → CRTC binding is the leased CRTC;
   - `GETFB`: framebuffer size, pitch, bpp, depth, and backing-handle presence;
   - the dumb buffer size/pitch, and a fresh re-map checksum of the pattern
     still in the buffer, match the request.
8. Teardown clears the CRTC (`set_crtc(None)`), destroys the framebuffer, then
   destroys the dumb buffer, then revokes the lease. `Drop` repeats teardown
   best effort. It clears rather than restores a previous compositor mode; a
   kernel fbdev console may reattach its own framebuffer after the lease is
   revoked, and the proof framebuffer is gone either way.

`GETFB` deliberately creates a **new** GEM handle in the caller’s file
(`drm_gem_fb_create_handle`), so the reported handle is not expected to equal
the dumb-buffer handle. Verification checks size/pitch/depth and a non-zero
backing handle, then closes the temporary handle so repeated verifies do not
accumulate handles.

### CLI

```text
ozma-multiseat --seat-drm-scanout [SEAT=]CARD:CONNECTOR \
               [--seat-drm-scanout-mode WIDTHxHEIGHT] [--json]
```

The proof runs before display/topology/plan work and exits. `--json` emits
`request`, final `scanout` status, retained `verification`, notes, and typed
`error`/`teardown_error`. The default planner/supervisor behavior is unchanged;
a seat prefix is accepted for CLI symmetry but the proof itself is one
standalone output.

### Typed errors

| Error | `code()` |
|---|---|
| `NoDrm` | `no_drm` |
| `NoPermission` | `no_permission` |
| `CapabilityRequired` | `capability_required` |
| `UnsupportedVkms` | `unsupported_vkms` |
| `RealCardRefused` | `real_card_refused` |
| `UnsupportedConnector` | `unsupported_connector` |
| `ModeFailed` | `mode_failed` |
| `BufferFailed` | `buffer_failed` |
| `FramebufferFailed` | `framebuffer_failed` |
| `VerificationFailed` | `verification_failed` |
| `Released`, `InvalidRequest`, `Io` | matching snake-case names |
| `Lease(DrmLeaseError)` | the underlying lease code |

## Tests

New `tests/drm_scanout.rs` (12 tests, fake only) covers:

- request parsing and explicit mode;
- auto selection refusing a real card while an explicit path is allowed;
- start → buffer/fill/framebuffer/`set_crtc`/page-flip event sequence;
- CRTC/framebuffer/buffer/checksum verification;
- teardown order (`clear_crtc`, `destroy_framebuffer`, `destroy_buffer`,
  `revoke`), idempotence, and drop-driven teardown;
- explicit and unknown mode selection;
- all injected typed failures and stable `code()` values;
- verification failure retained in status;
- non-fatal page-flip unavailability;
- status JSON with ids, notes, verification, and `released`;
- deterministic pattern fill with pitch padding.

New `tests/live_drm_scanout.rs` is `#[ignore]`d and gated by `OZMA_DRM_LIVE=1`.
It refuses any non-vkms card, programs the pattern, verifies through the handle,
re-reads the CRTC through a **second card fd**, checks the lessee fd is open
while active and closed after teardown, confirms the proof framebuffer is no
longer attached, and repeats the full cycle to prove objects/buffers are
reusable. `modetest` was not usable as evidence: with more than one vkms card,
`modetest -M vkms` opens the default instance regardless of `-D`, and libdrm
does not list configfs faux instances by busid. The test therefore uses the
same `GET_CRTC` ioctl directly.

## Live evidence

Host: Linux 7.3.5-2-cachyos. `/dev/dri/card0` = `xe`,
`card1` = `amdgpu`, `card2` = `hermes-kms`, `card3` = the default `vkms`
instance (`faux_driver`). A dedicated configfs instance
`/sys/kernel/config/vkms/ozma-final` was created for this run and systemd-logind
was briefly frozen (`SIGSTOP`) while the instance was enabled, because logind
otherwise holds DRM master on every newly tagged DRM card. No real GPU,
connector, compositor, or user session was mode-set or stopped.

Instance setup was a normal vkms configfs pipeline: one primary plane, one CRTC,
one encoder, one connected virtual connector, `enabled=1`.

```text
$ OZMA_DRM_LIVE=1 OZMA_DRM_CARD=/dev/dri/card4 \
    target/debug/deps/live_drm_scanout-... --ignored --nocapture
vkms card: card4 path=/dev/dri/card4 driver=faux_driver
connectors: [("Virtual-3", Some(43), None, Connected, ["1024x768", ...])]
crtcs: [41]
scanout status:
  connector_id=43 crtc_id=41 lessee_id=1 lessee_fd=4
  mode="1024x768" 1024x768 pitch=4096
  framebuffer_id=46 buffer_handle=1
  pattern="quadrants-border-diagonal-xrgb8888"
  pattern_checksum=3291019389 page_flipped=true applied=true
  notes=[scanout_proof_only, lessee_fd]
verification:
  crtc_id=41 crtc_mode="1024x768" crtc_framebuffer=46 position=(0,0)
  connector_state=connected connector_crtc=41
  framebuffer_id=46 size=(1024,768) pitch=4096 bpp=32 depth=24
  framebuffer_buffer_handle=2 (GETFB temp handle)
  buffer_size=(1024,768) buffer_pitch=4096 buffer_handle=1
  pattern_checksum=3291019389 ok=true
independent GET_CRTC: crtc 41 framebuffer=Some(46) mode=Some("1024x768") position=(0, 0)
released status: applied=false released=true lessee_fd=None
post-teardown GET_CRTC: crtc 41 framebuffer=Some(44) mode=Some("1024x768")
second scanout cycle completed and released
test result: ok. 1 passed; 0 failed; 0 ignored
```

The post-teardown `framebuffer=Some(44)` is the kernel fbdev console’s own
framebuffer being restored after the lease/master was released; the proof
framebuffer `46` is no longer attached. A second full start/verify/teardown
cycle succeeded on the same connector/CRTC, proving the lease, framebuffer, and
dumb buffer were released.

CLI proof (same instance, after the test released it):

```text
$ target/debug/ozma-multiseat --json --seat-drm-scanout /dev/dri/card4:Virtual-3
{
  "ok": true,
  ...
  "scanout": {
    "card": "/dev/dri/card4", "connector": "Virtual-3",
    "connector_id": 43, "crtc_id": 41,
    "lessee_id": 1, "mode": "1024x768",
    "width": 1024, "height": 768, "pitch": 4096,
    "framebuffer_id": 46, "buffer_handle": 1,
    "pattern": "quadrants-border-diagonal-xrgb8888",
    "pattern_checksum": 3291019389,
    "page_flipped": true, "applied": false, "released": true,
    "notes": [{"kind": "scanout_proof_only", ...}, {"kind": "lessee_fd", ...}]
  },
  "verification": {
    "crtc_id": 41, "crtc_mode": "1024x768", "crtc_framebuffer": 46,
    "connector_crtc": 41, "framebuffer_buffer_handle": 2,
    "ok": true
  }
}
```

Human form of the same proof:

```text
scanout: /dev/dri/card4:Virtual-3 connector_id=Some(43) crtc=Some(41) lessee=Some(1) fd=None
  mode=1024x768 1024x768 pitch=4096 framebuffer=Some(45) buffer=Some(1) \
  pattern=quadrants-border-diagonal-xrgb8888 checksum=Some(3291019389) page_flipped=true
  verify: ok=true crtc_mode=Some("1024x768") crtc_framebuffer=Some(45) \
  connector=connected connector_crtc=Some(41) framebuffer_buffer=Some(2) buffer_size=1024x768
  note[scanout_proof_only]: ...
  note[lessee_fd]: ...
  scanout applied=false released=true
```

(The two CLI runs created different framebuffer ids, 45 vs 46, because ids are
allocated per run; both were verified and destroyed.)

## Cleanup

After the runs:

```text
$ sudo ls /sys/kernel/config/vkms/
# empty
$ ls /sys/class/drm/ | grep -E '^card[0-9]+$'
card0
card1
card2
card3          # default vkms, not created by this run
$ ps -o stat,comm -p $(pidof systemd-logind)
Ss systemd-logind
```

The configfs instance and all its plane/CRTC/encoder/connector links were
removed, logind was resumed, and no temporary instance remains. The default
vkms card and the real GPUs were not touched.

## Verification

```text
cargo test -p ozma-multiseat
  206 passed, 0 failed, 14 ignored (includes the 12 fake scanout tests and
  the ignored live scanout test)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
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
- **No plane leasing or atomic commits.** The proof leases connector+CRTC and
  uses legacy `set_crtc`/`page_flip`. It does not select primary/overlay/cursor
  planes, use atomic property blobs, or test format modifiers.
- **No capture of the pattern.** vkms writeback output was not wired into this
  proof. Verification is KMS state (CRTC/connector/framebuffer/buffer ids, mode,
  size, pitch) plus a re-mapped checksum of the buffer bytes, not a read-back
  image.
- **No input or session integration.** `ozma-seat-input`/uinput isolation and
  the per-seat display backends are unchanged.
- **Host logind.** A default vkms card is often mastered by `systemd-logind`.
  The live run used a dedicated configfs instance and briefly froze logind; the
  tool itself reports `capability_required` rather than stealing a real
  session’s master.
- **Real-card policy.** An explicit `/dev/dri/cardN` may be used, but the
  operator must already be DRM master. Status records `real_gpu_named`; no
  automatic real-card scanout exists.
