# Phase 4 libvirt fixes report

Date: 2026-11-25 (Australia/Sydney; log timestamps UTC)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
Scope: `ozma-power`, `ozma-node` power/display-info wiring, docs.
Base report: [`phase4-libvirt-live-report.md`](phase4-libvirt-live-report.md)

## Verdict

The concrete gaps from the live libvirt run were fixed and re-validated against
a disposable `qemu:///system` domain.

| Gap | Result | Evidence |
|---|---|---|
| `--libvirt-uri` / `OZMA_LIBVIRT_URI` | **PASS** | Every `virsh` call used `--connect qemu:///system`; unset kept virsh's own default and reported the system domain as not found (session connection). |
| Invalid domain state | **PASS** | `/power/state` = 503 `{"status":"unavailable","code":"domain-not-found",...}`; actions returned 503 with the same typed reason. |
| ACPI `off` honesty | **PASS** | Still-running guest → HTTP 202, `pending:true`, `reason:"acpi-requested"`, `status:"running"`; `virsh domstate` also `running`. |
| `/display/info` geometry | **PASS** | Before frame: `width:0`, `height:0`, `dimensions_source:"unknown"`, `capability_max_width:1920`, `capability_max_height:1080`. After frame: `1280x800`, `last-frame`. |
| Cleanup | **PASS** | Disposable domain undefined; logs, sockets, temp dir, node processes and test ports removed; pre-existing domains/network/libvirtd unchanged. |

No host disk, GPU, USB or audio device was passed through, and no existing VM
or network was modified.

## 1. libvirt URI flag

### Change

- New node flag `--libvirt-uri <URI>` with environment variable
  `OZMA_LIBVIRT_URI`.
- `VmProviderConfig` carries `libvirt_uri` and `build_power` applies it with
  `LibvirtPower::with_uri`.
- `LibvirtPower` stores `uri: Option<String>`:
  - configured: `virsh --connect <uri> domstate|start|destroy|suspend|resume|reset <domain>`
  - unset: `virsh domstate|start|destroy|suspend|resume|reset <domain>` and
    virsh resolves its own default connection (historically `qemu:///session`
    for an unprivileged session).
- The backend never substitutes a default URI. `DEFAULT_LIBVIRT_URI` documents
  the observed unset behavior only.

### Unit coverage

`ozma-power/tests/libvirt_power.rs` asserts exact argv:

- state, pause, resume, cont-start, reset, stop with and without a URI;
- `virsh --connect qemu:///system` appears after `virsh` on every call;
- unset URI produces exactly `virsh <verb> <domain>`;
- ACPI shutdown argv includes the URI when configured.

### Live evidence

Configured flag:

```text
INFO ozma_power::libvirt: libvirt power: connected
     domain=ozma-phase4-fixes-20261125 uri="qemu:///system" state=running
INFO ozma_node: libvirt power backend connected
     domain="ozma-phase4-fixes-20261125" uri="qemu:///system"
GET /power/state -> 200 {"available":true,"status":"running"}
```

Environment variable `OZMA_LIBVIRT_URI=qemu:///system` without
`--libvirt-uri` produced the same connected log and state.

No URI, with `LIBVIRT_DEFAULT_URI` and `OZMA_LIBVIRT_URI` both unset, used the
session connection and therefore could not see the system domain:

```text
WARN ozma_node: libvirt power backend unavailable ... uri="<virsh-default>"
     kind="domain-not-found"
GET /power/state -> 503 {"available":false,"status":"unavailable",
                         "code":"domain-not-found",...}
```

This confirms the unset path keeps virsh's default instead of silently
pretending to use a system URI.

## 2. Invalid domain: typed unavailable state

### Change

- `PowerError::DomainNotFound { domain, stderr }` is produced only for virsh
  missing-domain messages (`failed to get domain`, `domain not found`,
  `no domain with matching name/id`). A missing `virsh` stays
  `missing-command`; a connection failure stays `command-failed`.
- `cont` does not retry `start` when the domain itself is missing.
- `PowerError::Unavailable { backend, kind, reason }` retains a startup
  connection failure; `UnavailablePower` replays it from every operation.
- `build_power` attaches `UnavailablePower` instead of returning `None`, so the
  node stays up but `/power/*` no longer says `not-configured`.
- `/power/state` reports HTTP 503, `status:"unavailable"`, the original
  `code`, and the original error. Actions return 503 with the same code.
  `/api/v1/connection` also carries `code` and `error` for a retained failure.

### Live evidence

```text
WARN ozma_node: libvirt power backend unavailable; /power/* will report the
     retained error domain="ozma-does-not-exist-20261125"
     uri="qemu:///system" kind="domain-not-found"
     error=libvirt domain "ozma-does-not-exist-20261125" was not found:
     error: failed to get domain 'ozma-does-not-exist-20261125'

GET  /power/state -> 503 {"available":false,"status":"unavailable",
                          "code":"domain-not-found",
                          "error":"libvirt power backend is unavailable: ..."}
POST /power/start -> 503 {"ok":false,"action":"on",
                          "code":"domain-not-found","error":"..."}
POST /power/off   -> 503 {"ok":false,"action":"off",
                          "code":"domain-not-found","error":"..."}
```

### Unit coverage

- missing domain → `DomainNotFound` with kind `domain-not-found`;
- connection failure → `CommandFailed` with kind `command-failed`, no
  missing-domain text;
- `UnavailablePower` replays `domain-not-found` for state and every action;
- controller routes: state and action return 503 with the retained reason;
- closed QMP remains the pre-existing 502 `qmp-closed` path.

## 3. ACPI `off` honesty

### Change

- New `PowerOutcome { state, pending, reason }` and
  `PowerBackend::system_powerdown_outcome` (default: accepted).
- `LibvirtPower::system_powerdown_outcome`:
  1. reads `domstate` first; an already-off domain is idempotent success and
     receives no shutdown request;
  2. sends `virsh shutdown`;
  3. polls `domstate` for a bounded 3 s (150 ms interval);
  4. returns `PowerOutcome::completed(shutoff)` only when `domstate` says off;
  5. otherwise returns `PowerOutcome::pending(observed_state,
     "acpi-requested")`.
- `POST /power/off` maps pending to HTTP 202 with `pending:true`,
  `reason:"acpi-requested"` and the real observed `status`; completed returns
  HTTP 200 with `status:"shutoff"`. A state that contradicts `domstate` is
  never returned.

### Live evidence

The Alpine guest ignored the ACPI request:

```text
POST /power/off -> 202
{"ok":true,"action":"off","pending":true,"reason":"acpi-requested",
 "status":"running"}
elapsed 3183 ms

GET /power/state -> 200 {"available":true,"status":"running"}
virsh domstate   -> running
```

Immediately after the request the node and libvirt agree the domain is still
running; the HTTP response says the request is pending, not that the VM is off.
A later `POST /power/stop` (`force-off`) returned 200 and `virsh domstate` =
`shut off`.

### Unit coverage

- immediate shutdown (`domstate` becomes `shut off`) → completed, 200
  semantics, shutdown argv observed;
- still running after the bounded poll → pending + `acpi-requested` + running,
  and no `destroy`/force-off argv;
- already off → completed shutoff with no shutdown request;
- controller `/power/off` pending → HTTP 202 body shape.

## 4. `/display/info` geometry

### Change

- `DisplayInfo.width` / `height` are now last-frame geometry only; before the
  first frame they are `0`.
- `dimensions_source` is `unknown` before a frame (`none` when no source).
- Capability limits are exposed separately as
  `capability_max_width` / `capability_max_height`.
- After a frame: `dimensions_source:"last-frame"` with real frame geometry.
- The display broker no longer fills pre-frame geometry from a
  `capability-max` state; explicitly advertised per-console display-output
  geometry still wins for a non-active console.

### Live evidence

Before the first frame:

```json
{
  "ok": true,
  "source_id": "qmp0",
  "snapshot_available": true,
  "dimensions_source": "unknown",
  "width": 0,
  "height": 0,
  "capability_max_width": 1920,
  "capability_max_height": 1080,
  "pixel_format": "rgb24"
}
```

`GET /display/snapshot` then returned a real `image/jpeg`, 96,470 bytes,
1280x800. After it:

```json
{
  "dimensions_source": "last-frame",
  "width": 1280,
  "height": 800,
  "capability_max_width": 1920,
  "capability_max_height": 1080
}
```

### Unit coverage

- `display_compat`: before first frame = zero/unknown + capability fields;
  after capture = last-frame;
- controller `/display/info`: HTTP JSON shape before and after a snapshot;
- display broker and parity tests still report last-frame after capture.

## 5. Validation commands

```text
cargo test -p ozma-power -p ozma-node --lib
cargo test -p ozma-power --test libvirt_power
cargo test -p ozma-node --bin ozma-node
cargo test -p ozma-node --test parity
cargo check -p ozma-power -p ozma-node --all-targets
rustfmt --edition 2021 --check <touched rust files>
```

Results:

- `ozma-node --lib`: 573 passed, 4 ignored.
- `ozma-power --test libvirt_power`: 25 passed.
- `ozma-node --bin ozma-node`: 21 passed.
- `ozma-node --test parity`: 22 passed.
- `cargo check --all-targets`: clean (only pre-existing `ozma-pipewire`
  warnings).
- `rustfmt --check`: clean on every touched Rust file.

## 6. Live re-validation and cleanup

Disposable system domain `ozma-phase4-fixes-20261125`:

- Alpine virt ISO read-only, 256 MiB, 1 vCPU, VGA, USB keyboard/tablet.
- No writable disk, no network interface, no host device passthrough.
- An extra QMP socket in `/tmp/opencode/ozma-libvirt-fixes` (chmod 0777 on the
  socket only so the unprivileged node could open it).

Exercised: state, pause/resume, ACPI `off` pending, force-off, start, snapshot
geometry before/after first frame, invalid-domain node, URI flag, URI env var,
and unset-URI default behavior.

Cleanup:

```text
virsh -c qemu:///system destroy ozma-phase4-fixes-20261125
virsh -c qemu:///system undefine ozma-phase4-fixes-20261125
rm -rf /tmp/opencode/ozma-libvirt-fixes
rm -f /var/log/libvirt/qemu/ozma-phase4-fixes-20261125.log
```

Post-cleanup checks:

- `virsh -c qemu:///system list --all`: only the five pre-existing shut-off
  domains remain.
- `virsh -c qemu:///session list --all`: empty.
- `virsh -c qemu:///system net-list --all`: `default active/yes/yes`.
- `systemctl is-active/is-enabled libvirtd`: `active` / `enabled`.
- No test `ozma-node` or `qemu-system` process, no test QMP socket, no test
  TCP/UDP listener, temp directory removed.

## 7. Remaining open gaps

These were out of scope for this fix pass and are unchanged:

1. **System-domain QMP socket permissions.** The extra `-qmp` socket is created
   mode 0775 owned by `libvirt-qemu`. The node correctly returns a typed
   capture error, but production still needs an ACL/umask strategy or a
   libvirt-managed monitor path.
2. **`qemu:commandline` is not a stable libvirt interface.** A supported way to
   expose a second QMP socket should be chosen and regression-tested.
3. **Single-client QMP socket.** The node's shared control client owns the
   socket; independent QMP probes time out while the node runs. Verification
   uses libvirt state and node logs.
4. **TCG only.** The disposable domains used `<domain type='qemu'>`; a KVM rerun
   would be useful for timing.

## Files touched

- `ozma-power/src/error.rs`
- `ozma-power/src/lib.rs`
- `ozma-power/src/libvirt.rs`
- `ozma-power/src/state.rs`
- `ozma-power/src/unavailable.rs` (new)
- `ozma-power/tests/libvirt_power.rs`
- `ozma-node/src/main.rs`
- `ozma-node/src/config.rs`
- `ozma-node/src/controller_compat.rs`
- `ozma-node/src/display_compat.rs`
- `ozma-node/src/display_broker.rs`
- `ozma-node/src/platform.rs`
- `docs/phase4-libvirt-live-report.md`
- `docs/phase4-libvirt-fixes-report.md` (this file)
