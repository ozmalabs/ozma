# Phase 4 — Supervisor child-argv parity

Status: implemented in `ozma-softnode`, no default behavior change for existing
deployments. This document records the flag matrix, the argv-builder semantics,
the status surface, and the parity gaps that remain. A second parity pass added
the UAC2 bridge, VNC server, read-only multiseat report, and node-local mesh
flags; see `docs/phase4-supervisor-new-flags-report.md` for that pass's details.

Scope: `ozma-softnode/src/{supervisor,main,manager,status}.rs` only. `ozma-node`
and installers were not modified.

## Why

The Rust supervisor used to build a fixed child argv:

```text
ozma-node --profile vm --name <vm> --qmp-socket <path>
  [--qmp-input-socket <path>]
  --hid-udp-port <n> --http-port <n>
  --capture-provider qmp --input-provider qmp --power-provider qmp
  [--register-url <url>] [--audio-sink <sink>]
```

That argv cannot produce HLS, a display picker, WebRTC, VNC capture,
USB/media, or GPU-passthrough capture, even though `ozma-node` supports all of
them. The packaged systemd path therefore was not demo-equivalent.

The supervisor now has one typed argv builder
(`supervisor::build_child_argv`) and a `Vec<String>`-returning wrapper
(`build_node_spec_with_options`). Every flag is emitted from that builder;
`build_node_spec` remains as a default-options wrapper for existing tests and
callers.

## Core argv (always emitted)

| Child flag | Source | Notes |
|---|---|---|
| `--profile vm` | fixed | Per-VM supervisor profile. |
| `--name <vm>` | discovery | VM name. |
| `--qmp-socket <path>` | discovery/provisioning | Never fabricated; the manager only spawns when a socket exists. |
| `--qmp-input-socket <path>` | discovery/provisioning | Only when a dedicated input socket was proven. |
| `--hid-udp-port <n>` | allocator | Stable per VM. |
| `--http-port <n>` | allocator | Stable per VM. |
| `--capture-provider <provider>` | builder | `qmp` unless VNC capture or Looking Glass applies. |
| `--input-provider qmp` | fixed | VM input targets the guest, not hypervisor uinput. |
| `--power-provider qmp` | fixed | libvirt power remains a gap (see below). |
| `--register-url <url>` | `--controller` | Omitted when empty. |
| `--audio-sink <sink>` | audio provisioning | Omitted when no per-VM sink was created. |

## Opt-in forwarding matrix

All options are off by default. Default options produce exactly the former
argv.

### HLS / codec

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--hls`, `--multi-console-hls` | `--hls` | `--multi-console-hls` also enables `--hls`. |
| `--hls-dir <root>` | `--hls-dir <root>/<vm>` | VM component is sanitized. |
| `--hls-fps <n>` | `--hls-fps <n>` | |
| `--hls-bitrate-kbps <n>` | `--hls-bitrate-kbps <n>` | |
| `--hls-encoder <name>` | `--hls-encoder <name>` | Supervisor-level codec selector. |
| `--hls-pixel-format <fmt>` | `--hls-pixel-format <fmt>` | |
| `--hls-10bit`, `--hls-hdr` | `--hls-10bit`, `--hls-hdr` | |
| `--hls-color-primaries/-transfer/-matrix` | same | HDR metadata. |
| `--multi-console-hls` | `--multi-console-hls` | |
| `--multi-console-hls-max <n>` | `--multi-console-hls-max <n>` | |
| `--multi-console-hls-idle-seconds <n>` | `--multi-console-hls-idle-seconds <n>` | |

### Display / guest IP

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--display-output VM=SPEC` (repeatable) | `--display-output SPEC` | Forwarded only to that VM. `SPEC` is `LABEL:WxH[:PRIMARY][:SOURCE]` or `INDEX:LABEL:WxH...`. |
| (discovery-derived) | `--display-output SPEC` | Used when discovery populated `VMInfo.displays` and no explicit mapping exists. Discovery does not populate it yet. |
| `--vm-guest-ip VM=IP` (repeatable) | `--vm-guest-ip IP` | Validated as an IP address. |

### VNC capture

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--vnc-capture` | `--capture-provider vnc`, `--vnc-host <host>`, `--vnc-port <port>` | Only for a non-passthrough VM with a discovered VNC port. GPU-passthrough VMs are never switched to VNC. |

This is **capture**, not `vnc_host`/`vnc_port` registration with the
controller; `ozma-node` has no registration field for those yet.

### VNC server (RFB)

`--vnc-server` is independent of `--vnc-capture`: it rebroadcasts the child's
own capture source (QMP, external VNC, Looking Glass, ...) as an RFB 3.8
endpoint.

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--vnc-server` | `--vnc-server` | Forwarded to every child unless `--vnc-vm` scopes it. |
| `--vnc-bind <addr>` | `--vnc-bind <addr>` | Requires `--vnc-server`. Loopback is the node default; a non-loopback bind is the explicit opt-in. |
| `--vnc-server-port <n>` | `--vnc-server-port <n>` | Requires `--vnc-server`. `0`/unset is omitted so each child gets an OS-assigned port. |
| `--vnc-vm <vm>` | (selector) | Scopes all `--vnc-server*` flags to one VM. Required for a non-zero port. |

Port allocation rule: the supervisor never gives two children the same fixed
TCP port. With no `--vnc-vm`, every child keeps the node's default `0` and the
OS assigns a distinct port, which the node resolves and registers. A non-zero
`--vnc-server-port` is accepted only together with `--vnc-vm`; otherwise
validation fails with a typed error. Security remains RFB `None` (no password,
TLS, or authentication).

### Multiseat report

The node's multiseat surface is a **read-only** report; seat lifecycle stays
with the standalone `ozma-multiseat` supervisor. The persisted `seats.json`
store is shared host state, so exactly one supervised child may own it.

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--multiseat` | `--multiseat` | Requires `--seats-file` and `--multiseat-vm`. |
| `--seats-file <path>` | `--seats-file <path>` | Forwarded only to the selected VM. |
| `--multiseat-vm <vm>` | (selector) | The single logical node that serves `GET /api/v1/seats`. |

`--seats-file` without `--multiseat` and `--multiseat` without a selector are
typed `InvalidOption` errors, not silent no-ops.

### Mesh

A mesh identity (mesh IPv4, WireGuard UDP port, private key) is unique per node.
Forwarding the same configuration to every per-VM child would bind the same UDP
port and advertise the same identity several times, so mesh flags are scoped to
one selected child.

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--mesh` | `--mesh` | Requires `--mesh-vm`. |
| `--mesh-ip <ipv4>` | `--mesh-ip <ipv4>` | |
| `--mesh-port <n>` | `--mesh-port <n>` | Non-zero; omitted leaves the node default `51820`. |
| `--mesh-key-file <path>` | `--mesh-key-file <path>` | Only the path is forwarded; the supervisor never reads key material. |
| `--mesh-kernel` | `--mesh-kernel` | Forwarded; the node reports kernel WireGuard as typed unsupported and stays on userspace boringtun. |
| `--mesh-peer <json>` (repeatable) | `--mesh-peer <json>` | One child flag per peer, like `--display-output`. |
| `--mesh-vm <vm>` | (selector) | The single child that owns this mesh identity. |

Any mesh sub-option without `--mesh`, `--mesh` without `--mesh-vm`, and
`--mesh-port 0` are typed `InvalidOption` errors.

### WebRTC settle

There is no `--webrtc-settle-ms` in `ozma-node` as of this pass (no
`settle` flag exists in the node CLI or WebRTC module), so the supervisor does
not forward one. Nothing is invented for parity.

### WebRTC

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--webrtc-helper <path>` | `--webrtc-helper <path>` | Presence enables WebRTC forwarding. |
| `--webrtc-helper-script <path>` | `--webrtc-helper-script <path>` | |
| `--webrtc-socket-dir <dir>` | `--webrtc-socket <dir>/<vm>.sock` | Per VM, so children cannot collide. |
| `--webrtc-shm-dir <dir>` | `--webrtc-shm <dir>/<vm>.shm` | Per VM. |
| `--webrtc-width/-height/-fps/-bitrate-kbps` | same | |
| `--webrtc-fit-capture` | `--webrtc-fit-capture` | Uses each VM's runtime capture size; needed for QMP/VNC heads. |
| `--webrtc-audio-from-sink` | `--webrtc-audio-source <sink>.monitor` | Only when a per-VM audio sink exists. |
| `--webrtc-audio-backend <auto\|pulse\|pipewire>` | `--webrtc-audio-backend <value>` | |
| `--webrtc-stun <uri>` | `--webrtc-stun <uri>` | |
| `--webrtc-turn <uri>` | `--webrtc-turn <uri>` | |
| `--webrtc-turn-user <user>` | `--webrtc-turn-user <user>` | Redacted from status/logs. |
| `--webrtc-turn-password <secret>` | `--webrtc-turn-password <secret>` | Redacted from status/logs. |
| `--multi-console-webrtc` + max/idle | same | |

Other `--webrtc-*` options require `--webrtc-helper`; otherwise validation
fails with a typed error instead of silently emitting a half-configured helper.

### USB / media

USB gadget state is host-scoped. Forwarding it to every child would make
several children race for one UDC, so the supervisor forwards it to exactly one
named VM.

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--usb-gadget` | `--usb-gadget` | Requires `--usb-vm` and `--usb-udc`. |
| `--usb-vm <vm>` | (selector) | Only this VM's child gets the USB flags. |
| `--usb-udc <udc>` | `--usb-udc <udc>` | |
| `--usb-configfs-root <path>` | `--usb-configfs-root <path>` | |
| `--usb-media <path>` | `--usb-media <path>` | |
| `--usb-media-ro`, `--usb-media-cdrom` | same | |
| `--usb-uac2`, `--usb-vendor` | same | |
| `--usb-uac2-bridge` | `--usb-uac2-bridge` | Requires `--usb-uac2`; starts the ALSA isochronous bridge. |
| `--usb-uac2-sink-device <alsa>` | `--usb-uac2-sink-device <alsa>` | Requires the bridge. Omitted leaves the node default `default`. |
| `--usb-uac2-source-device <alsa>` | `--usb-uac2-source-device <alsa>` | Requires the bridge. Omitted leaves the node default `default`. |
| `--usb-vendor-daemon <path>` | `--usb-vendor-daemon <path>` | |

The UAC2 bridge only starts when the gadget's ALSA card and host audio devices
exist; otherwise the node keeps `/usb` at a typed `unsupported` bridge. The
supervisor forwards configuration but never claims live PCM.

### GPU passthrough / Looking Glass

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--looking-glass-shm VM=PATH` (repeatable) | `--capture-provider looking-glass`, `--looking-glass-shm <path>` | Only for a VM discovery marks `has_gpu_passthrough`. |
| `--looking-glass-fps/-id/-name` | same | Forwarded with the shm path. |

A GPU-passthrough VM without a `--looking-glass-shm` mapping keeps qmp capture
and receives a status/log warning; the supervisor does not claim success for
the in-guest agent capture path, which it does not supervise.

### Audio routing

| Supervisor CLI | Child flag(s) | Mapping |
|---|---|---|
| `--audio-router <none\|pipewire\|pulseaudio>` | `--audio-router <value>` | Emitted whenever configured; it selects node API routing, sink or not. |
| `--audio-capture-from-sink` | `--audio-capture-target monitor:<sink>` | Only when a sink is passed; otherwise a status warning is recorded. |
| `--audio-track-root <path>` | `--audio-track-root <path>` | |

`--audio-router` is also used by the supervisor's own audio provisioning; the
same value is forwarded to children. There is still no `--audio-type` flag in
`ozma-node`; a non-empty `--audio-sink` advertises `pipewire`.

## Argv builder semantics

`supervisor::build_child_argv(vm, ports, register_url, audio_sink, options,
support, policy) -> Result<NodeArgv, ChildArgvError>`

- **Single source of truth.** All flags for a per-VM child come from this
  function. `build_node_spec` and `build_node_spec_with_options` only attach
  the program, ports, socket, sink, and warnings.
- **Configured only.** Every opt-in flag is emitted only when its option is
  `Some`/enabled and non-empty. Empty strings are never emitted, whether they
  came from CLI values or config.
- **Exactly once.** Flags are emitted in a fixed order and never repeated
  except `--display-output` (one per display) and `--mesh-peer` (one per
  configured peer), which are intentionally repeatable.
- **Single-owner flags.** USB/media, the VNC server when `--vnc-vm` is set, the
  multiseat report, and the mesh identity are forwarded to one selected child
  only. Their configuration is host- or identity-scoped and cannot be shared by
  sibling processes; misuse is a typed `InvalidOption` before any child starts.
- **Unsupported target flags.** `NodeFlagSupport::from_help` parses the target
  `ozma-node --help`; `probe_node_flags` runs it once at startup. A configured
  flag the target does not advertise is
  - omitted and recorded as a `NodeArgvWarning` under
    `UnsupportedPolicy::Warn` (runtime), or
  - rejected with `ChildArgvError::UnsupportedFlag` under
    `UnsupportedPolicy::Reject` (tests/config validation).
- **Unknown target.** `NodeFlagSupport::Unknown` assumes this crate's flags;
  this preserves old deployments when the binary cannot be probed. Probe
  failure is logged, and status still contains the exact argv.
- **Runtime policy.** `--reject-unsupported-node-flags` switches the runtime
  from warn-and-omit to `UnsupportedPolicy::Reject`: that VM's child is not
  spawned, `last_error` carries the typed `UnsupportedFlag` message, and the
  status state is `argv-error`.
- **Typed config errors.** Invalid combinations (HLS sub-options without HLS,
  WebRTC options without a helper, USB without `--usb-vm`/`--usb-udc`,
  `--usb-uac2-bridge` without `--usb-uac2`, UAC2 device flags without the
  bridge, `--vnc-bind`/`--vnc-server-port`/`--vnc-vm` without `--vnc-server`, a
  non-zero VNC port without `--vnc-vm`, multiseat without a seats file or
  selector, mesh sub-options without `--mesh`, `--mesh` without `--mesh-vm`,
  `--mesh-port 0`, a non-IP guest IP, malformed `--display-output`, invalid
  router/backend values) return `ChildArgvError::InvalidOption` from
  `ChildOptions::validate`.
- **Per-VM file names.** HLS directories, WebRTC sockets, and shm paths replace
  path separators/whitespace in VM names rather than trusting hypervisor names.

`--no-node-help-probe` disables the startup `ozma-node --help` probe. Probing
starts no VM and opens no socket.

## Status and diagnostics

Each `VmStatus` in `GET /status` now includes:

```json
{
  "child_argv": ["ozma-node", "--profile", "vm", "..."],
  "argv_warnings": ["--hls was not forwarded to the child (--hls): ..."]
}
```

- `child_argv` is the exact argv the supervisor would spawn, with the value of
  every secret flag replaced by `<redacted>`.
- Secret flags are `--api-token`, `--webrtc-turn-user`, and
  `--webrtc-turn-password`. Both `--flag value` and `--flag=value` forms are
  handled. `--mesh-key-file` carries only a path; the supervisor never reads or
  serializes the private-key contents.
- `argv_warnings` names every configured option that was not forwarded and why.
- Under `--reject-unsupported-node-flags`, a rejected option produces no child
  and the VM state is `argv-error` with the typed message in `last_error`.
- Neither field is added when empty, so existing status consumers are
  unaffected.
- Metrics remain numeric/label-only and do not include argv or paths.

## Tests

`cargo test -p ozma-softnode` covers:

- an argv parity table asserting each supported flag appears exactly once and
  key values (per-VM HLS dir, display spec, guest IP, WebRTC socket/shm/audio,
  Looking Glass shm, USB media, UAC2 devices, VNC server bind/port, seats file,
  mesh IP/port/key/peer);
- absent options omitted with the exact legacy core argv;
- per-VM/per-node scoping for VNC server, multiseat, mesh, display, and USB;
- VNC capture selected only for non-passthrough VMs;
- typed `UnsupportedFlag` under reject and warn-and-omit under warn, including
  the new VNC-server and mesh flags;
- typed `InvalidOption` for USB without a VM, WebRTC without a helper, a
  non-zero VNC port without `--vnc-vm`, multiseat without a seats file or
  selector, and mesh without a selector or with port `0`;
- secret redaction for `--flag value` and `--flag=value`;
- a fake-child process test that launches a real script through
  `ProcessNodeLauncher` and compares the script's received argv (including the
  VNC server and mesh flags) with the built argv;
- a manager status test asserting `child_argv` is redacted and
  `argv_warnings` reports an unsupported `--hls`;
- a manager reject-policy test asserting no child spawns and status reports
  `argv-error` with the typed message;
- CLI parsing and duplicate/empty `VM=VALUE` rejection.

## Remaining parity gaps

These are **not** claimed as fixed:

1. **VNC registration.** `vnc_host`/`vnc_port` are still not registered with
   the controller. `--vnc-capture` only selects VNC as the capture provider.
2. **GPU-passthrough agent capture.** Without a Looking Glass shm path the
   supervisor warns and keeps qmp capture; the in-guest agent frame path is not
   supervised from the hypervisor.
3. **USB is single-VM.** The supervisor does not create per-VM gadgets or
   coordinate multiple UDCs. Operators must scope one `--usb-vm`. The UAC2
   bridge is likewise forwarded only to that VM and is not validated against
   real UDC/ALSA hardware from the supervisor.
4. **Power provider remains `qmp`.** Python `virtual_node.py` preferred
   `LibvirtPower`; the Rust supervisor does not forward `--libvirt-domain`.
5. **No PCI-device registration.** `--pci-devices-json` / PCI passthrough
   metadata is not forwarded. Discovery's `has_gpu_passthrough` only selects
   Looking Glass capture.
6. **Display discovery.** `VMInfo.displays` is not populated by the
   Proxmox/libvirt/QMP discovery backends yet, so display outputs currently
   require explicit `--display-output VM=...`.
7. **No seat lifecycle.** The multiseat flags only forward the node's read-only
   report for one selected VM. The supervisor never starts, stops, or restarts
   seats; that remains the standalone `ozma-multiseat` supervisor's job.
8. **No host/registration metadata forwarding.** `--register-host`, `--role`,
   `--machine-class`, `--hw`, `--fw`, `--heartbeat-interval`, `--cap`, and
   `--pci-devices-json` are not exposed by the supervisor.
9. **No RTSP/WebTransport listeners.** `--rtsp-port`,
   `--webtransport-port`, and `--webtransport-bind` are not forwarded.
10. **WebRTC helper ownership.** The helper is still the Python GStreamer
    worker; moving or replacing it is the separate P0 item in
    `docs/phase4-cutover-audit.md`.
11. **Child API token.** The supervisor's `--child-api-token` is for polling
    child APIs. `ozma-node --api-token` is not synthesized; an inherited
    `OZMA_API_TOKEN` environment still reaches the child, and redaction covers
    `--api-token` if it is ever added.
12. **Codec beyond HLS.** There is no separate node-wide codec flag; the
    supervisor's codec choice is `--hls-encoder`.
13. **VNC server is single-owner only for a fixed port.** A fixed
    `--vnc-server-port` requires `--vnc-vm`; the supervisor does not allocate a
    range of VNC ports per VM. Multi-VM hosts should leave the port unset and
    rely on OS-assigned ports.
14. **Mesh is one identity per supervisor.** The supervisor does not derive a
    distinct mesh IP/port/key per VM, browse mesh peers, initiate handshakes, or
    route a dataplane. It forwards one operator-supplied identity to one child.
15. **No AIDA64 forwarding.** AIDA64 node flags are intentionally out of scope
    for this pass and are not emitted.
16. **No WebRTC settle flag.** `ozma-node` has no `--webrtc-settle-ms` yet, so
    the supervisor has no settle/fit-time parity to forward.

## Verification

Run from the repository root:

```bash
cargo test -p ozma-softnode
cargo check -p ozma-softnode --all-targets
rustfmt --edition 2021 --check \
  ozma-softnode/src/supervisor.rs \
  ozma-softnode/src/main.rs \
  ozma-softnode/src/manager.rs \
  ozma-softnode/src/status.rs
```

The probe path can be checked cheaply against a built binary:

```bash
cargo run -p ozma-node -- --help
```

No QEMU/VM is started by the probe or the tests.
