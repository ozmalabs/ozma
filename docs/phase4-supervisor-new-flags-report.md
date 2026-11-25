# Phase 4 — Supervisor parity for UAC2, VNC server, multiseat, and mesh

Status: implemented in `ozma-softnode` only. No `ozma-node`, `ozma-usb`, or
`ozma-multiseat` source was changed. No commit was made. Default argv is
unchanged; every flag below is opt-in.

This is the second supervisor parity pass. The first pass is documented in
`docs/phase4-supervisor-parity.md`.

## Scope

Files changed:

- `ozma-softnode/src/supervisor.rs` — typed option structs, validation, argv
  emission, and tests.
- `ozma-softnode/src/main.rs` — CLI flags, wiring, and tests.
- `docs/phase4-supervisor-parity.md` — matrix and gap updates.
- This report.

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not edited.
`ozma-node` was read for authoritative flag names/defaults only. AIDA64 flags
being added in another session were ignored.

## New flag matrix

### USB UAC2 bridge (single owner: `--usb-vm`)

| Supervisor CLI | Child flag | Default / rule |
|---|---|---|
| `--usb-uac2-bridge` | `--usb-uac2-bridge` | Requires `--usb-uac2`. Forwarded only to `--usb-vm`. |
| `--usb-uac2-sink-device <alsa>` | `--usb-uac2-sink-device <alsa>` | Requires the bridge. Omitted leaves the node's `default`. |
| `--usb-uac2-source-device <alsa>` | `--usb-uac2-source-device <alsa>` | Requires the bridge. Omitted leaves the node's `default`. |

The supervisor forwards configuration only. The node still starts the bridge
only when the UAC2 gadget's ALSA card and host audio devices exist; otherwise
`/usb` reports a typed `unsupported` bridge. No live-PCM success is claimed.

### VNC server (multi-VM by default; fixed port is single owner)

| Supervisor CLI | Child flag | Default / rule |
|---|---|---|
| `--vnc-server` | `--vnc-server` | Forwarded to every child unless scoped. Independent of `--vnc-capture`. |
| `--vnc-bind <addr>` | `--vnc-bind <addr>` | Requires `--vnc-server`; node default is loopback. |
| `--vnc-server-port <n>` | `--vnc-server-port <n>` | Requires `--vnc-server`. `0`/unset is omitted. Non-zero requires `--vnc-vm`. |
| `--vnc-vm <vm>` | (selector) | Scopes every `--vnc-server*` flag to one child. |

**Port allocation:** the supervisor does not hand out a VNC port range. With no
selector, every child keeps the node default `0`; the OS assigns a distinct
port per child and the node registers the resolved port. A fixed non-zero port
requires `--vnc-vm`, because two children cannot bind one TCP port. Security is
RFB `None` only (no password, TLS, or authentication).

### Multiseat report (single logical node)

| Supervisor CLI | Child flag | Default / rule |
|---|---|---|
| `--multiseat` | `--multiseat` | Requires `--seats-file` and `--multiseat-vm`. |
| `--seats-file <path>` | `--seats-file <path>` | Forwarded only to the selected VM. |
| `--multiseat-vm <vm>` | (selector) | Exactly one child serves `GET /api/v1/seats`. |

The node's multiseat surface is read-only. The supervisor never starts, stops,
or restarts seats; that stays with the standalone `ozma-multiseat` supervisor.
`--seats-file` without `--multiseat` is a typed error rather than a silent
no-op.

### Mesh (one identity per supervisor)

| Supervisor CLI | Child flag | Default / rule |
|---|---|---|
| `--mesh` | `--mesh` | Requires `--mesh-vm`. |
| `--mesh-ip <ipv4>` | `--mesh-ip <ipv4>` | |
| `--mesh-port <n>` | `--mesh-port <n>` | Must be non-zero; omitted leaves `51820`. |
| `--mesh-key-file <path>` | `--mesh-key-file <path>` | Path only; key material is never read by the supervisor. |
| `--mesh-kernel` | `--mesh-kernel` | Forwarded. The node reports kernel mode as typed unsupported and uses userspace boringtun. |
| `--mesh-peer <json>` | `--mesh-peer <json>` | Repeatable, one child flag per peer. |
| `--mesh-vm <vm>` | (selector) | The single child that owns the mesh identity. |

A mesh identity is unique per node. Sending one identity to several children
would make them race for the same UDP port and advertise the same key, so the
supervisor requires one owner and rejects any mesh sub-option without `--mesh`,
`--mesh` without `--mesh-vm`, and `--mesh-port 0`.

### WebRTC settle

`ozma-node` has no `--webrtc-settle-ms` (or any `settle` flag) at the time of
this pass. The supervisor does not forward one and makes no claim about it.
When the node gains the flag, it only needs a typed option, builder push, and a
row in the parity table.

## Scoping rules

- **Per-VM:** HLS dirs, display outputs, guest IPs, WebRTC socket/shm paths,
  Looking Glass mappings, and USB are unchanged from the first pass.
- **Single VM (`--usb-vm`):** USB gadget/media and the UAC2 bridge. One child
  binds the UDC and may open the gadget's ALSA card.
- **Single VM (`--vnc-vm`):** the VNC server only when a fixed port is needed.
  Without the selector, `--vnc-server`/`--vnc-bind` go to every child and ports
  stay OS-assigned.
- **Single logical node (`--multiseat-vm`):** the read-only seat report. The
  shared `seats.json` is not concurrently loaded by sibling children.
- **Single logical node (`--mesh-vm`):** the mesh identity, UDP port, and key
  file.
- **Every child:** nothing new. All new flags are opt-in.

## Validation (typed, before spawn)

`ChildOptions::validate` returns `ChildArgvError::InvalidOption` for:

- `--usb-uac2-bridge` without `--usb-uac2`;
- UAC2 sink/source device flags without `--usb-uac2-bridge`, or empty device
  names;
- `--vnc-bind`, `--vnc-server-port`, or `--vnc-vm` without `--vnc-server`;
- a non-zero `--vnc-server-port` without `--vnc-vm`;
- `--multiseat` without `--multiseat-vm` or `--seats-file`;
- `--seats-file` or `--multiseat-vm` without `--multiseat`;
- any mesh sub-option without `--mesh`;
- `--mesh` without `--mesh-vm`, `--mesh-port 0`, or an empty `--mesh-peer`.

Flags the target `ozma-node --help` probe does not advertise still follow the
existing warn/omit policy, or `--reject-unsupported-node-flags` for a typed
`UnsupportedFlag` and an `argv-error` VM state.

## Tests

Added or extended in `ozma-softnode`:

- parity table now asserts the UAC2 bridge/sink/source, VNC server/bind/port,
  multiseat/seats file, and mesh IP/port/key/peer flags appear exactly once
  with the expected values;
- absent-option test asserts all new flags are omitted by default;
- per-VM/per-node scoping test asserts a non-selected VM receives none of the
  VNC-server, multiseat, or mesh flags;
- `new_server_and_mesh_flags_follow_warn_and_reject_policy` asserts typed
  reject and warn/omit for the new flags;
- `new_flag_combinations_are_typed_errors` covers every rule listed above;
- `new_child_feature_flags_parse` covers CLI parsing and validates the resulting
  `ChildOptions`;
- the fake-child launch test now includes VNC-server and mesh flags and still
  compares the child's received argv byte-for-byte with the built argv;
- existing secret-redaction tests remain unchanged and pass.

Results:

```text
cargo test -p ozma-softnode
  128 passed; 0 failed

cargo check -p ozma-softnode --all-targets
  ok (pre-existing dead-code warnings only)

rustfmt --edition 2021 --check \
  ozma-softnode/src/supervisor.rs ozma-softnode/src/main.rs
  no diff
```

## Remaining gaps

- **VNC registration metadata** (`vnc_host`/`vnc_port` as registration fields)
  is still separate from `--vnc-server`; the server path is now forwarded.
- **No VNC port range allocator.** Multi-VM hosts must leave the port at `0`;
  a fixed port is single-VM by design.
- **Multiseat lifecycle** stays external. The supervisor reports, never
  starts/stops seats.
- **Mesh is one identity.** No per-VM mesh IP/port/key allocation, peer
  browsing, handshake initiation, dataplane routing, or kernel WireGuard.
- **UAC2 hardware validation** is not performed by the supervisor; the node's
  typed `unsupported`/`configured`/`running` report is the source of truth.
- **AIDA64 flags** are intentionally not forwarded.
- **No `--webrtc-settle-ms`** exists in the node yet.
