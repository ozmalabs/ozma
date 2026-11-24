# ozma-softnode

`ozma-softnode` is the hypervisor-side supervisor. It discovers QEMU/KVM VMs
through Proxmox, libvirt (`virsh`), or a QMP socket scan, and supervises one
real `ozma-node --profile vm` subprocess per VM that has a usable QMP socket.

This crate does **not** implement the per-VM runtime itself, and it never
spawns an `ozma-softnode` binary. The child binary defaults to `ozma-node` and
is configurable with `--node-bin`.

## What the supervisor does

For every discovered VM with a usable QMP socket:

* allocates a stable, distinct HID UDP port (`--base-port`, default `7332`) and
  HTTP port (`--node-http-base`, default `7382`);
* launches:

  ```text
  ozma-node --profile vm \
    --name <vm.name> \
    --qmp-socket <vm.qmp_path> \
    [--qmp-input-socket <vm.qmp_input_path>] \
    --hid-udp-port <allocated> \
    --http-port <allocated> \
    --capture-provider qmp \
    --input-provider qmp \
    --power-provider qmp \
    [--audio-sink <provisioned sink>] \
    [--register-url <controller>]
  ```

`--input-provider qmp` is always explicit. A dedicated `--qmp-input-socket` is
added when the VM model or provisioner exposes one; otherwise `ozma-qmp` uses
the control socket in single-socket mode. This keeps input on the guest QMP path
instead of the VM profile's host-side `uinput` default.

* starts new VMs, stops removed VMs, and leaves unchanged VMs alone;
* restarts a failed child with bounded exponential backoff
  (`--restart-backoff-ms`, default `1000`; `--restart-backoff-max-ms`, default
  `30000`); and
* stops every child and waits for it on SIGINT/SIGTERM.

A VM whose discovered `qmp_path` is empty is tracked for status/agent work but
is **not** started, and no socket path is fabricated. A warning is logged. That
is the default. `--provision` adds the opt-in libvirt path described below.

`--no-auto-manage` keeps discovery-only operation. `--no-auto-agent` keeps the
existing switch for in-guest agent provisioning.

## Aggregated status and metrics

`--status-port <u16>` (default `0`, disabled) starts a small read-only HTTP
listener for the whole supervisor. `0.0.0.0` is used so the controller or a
Prometheus scraper can reach it; the surface is read-only and never opens QMP
sockets, probes children, or runs `virsh` while answering. The listener is
unauthenticated: `/status` exposes VM names and QMP socket paths (not secrets),
so restrict it with the same network policy as the per-VM node ports.

| Route | Content |
|---|---|
| `GET /health` | `{ok, version, profile, discovered, running, failed, skipped}` |
| `GET /status` | Full aggregated document: supervisor totals plus one entry per tracked and excluded VM and optional cached child summaries |
| `GET /metrics` | Prometheus text (`text/plain; version=0.0.4`) |
| `GET /api/v1/connection` | Connection-shaped summary: `ok`, `node_id`, `profile: "hypervisor"`, `version`, `api_port`, `quic_port: 0`, `hid_udp_port: 0`, `sink_kind: "supervisor"`, `supervisor` totals |

`profile` is always `hypervisor` and `node_id` is `ozma-virtual-node`; per-VM
ports and child details live in `/status`. A `status_port` of `0` starts no
listener and is not an error.

### `/status` schema

`supervisor` totals:

| Field | Meaning |
|---|---|
| `discovered` | VMs in the last discovery snapshot, including excluded ones |
| `tracked` | Discovered VMs the supervisor tracks (excluding `--exclude` matches) |
| `running` | VMs with a live child process |
| `failed` | VMs whose child is waiting to restart, or whose provisioning failed |
| `skipped` | Tracked VMs with no child (no QMP socket, awaiting restart, or discovery-only) |
| `excluded` | VMs filtered by `--exclude` |
| `next_hid_port`, `next_http_port` | Port pair the next new VM would receive, when available |

Each `vms[]` entry has `name`, `state`, `qmp_socket`, `hid_udp_port`,
`http_port`, `child_pid`, `spawned`, `skipped`, `needs_restart`, `excluded`,
`restart_count`, `restart_in_ms`, `last_exit`, `last_error`, `evdev_status`,
`evdev_keyboard`, `evdev_mouse`, `audio_sink`, `agent_status`, `agent_checks`,
and `child`.

`state` is one of `running`, `starting`, `backoff`, `stopped`, `skipped`,
`needs-restart`, `awaiting-socket`, `failed`, `discovery-only`, or `excluded`.
`restart_count` counts successful starts after the initial spawn; `last_exit`
is the most recent exit (`exit code 1`, `signal`, ...); `last_error` is cleared
by a successful start. `agent_status` is `provisioned`, `pending`, `retrying`,
`failed`, or `disabled`. `evdev_status` is `ready`, `unavailable`, or
`not-configured`.

### Per-child API aggregation (from `--child-poll-interval-ms`)

`--child-poll-interval-ms <u64>` (default `0`, disabled) polls each **running**
child's `GET /api/v1/connection` on loopback. `--child-poll-timeout-ms`
(default `2000`) bounds each request. `--child-api-token` (or
`OZMA_NODE_API_TOKEN`) is sent as a bearer token when child APIs are guarded;
the token is never serialized or logged. Setting the interval to `0` disables
polling cleanly: no task is spawned and `vms[].child` is `null`.

Polling runs in its own task and does not block discovery, restart handling, or
`/status`. The status read only copies the in-memory cache. Each `child` entry
is a bounded summary:

| Field | Meaning |
|---|---|
| `reachable` | Most recent poll reached the child, it reported `ok:true`, and the child is still spawned |
| `node_id`, `profile`, `api_port` | Child identity and API port |
| `hls`, `webrtc`, `audio` | Child state strings (`running`, `not-configured`, `failed`) |
| `power` | `{state, available}`; a string-only power value becomes `available:false` |
| `input` | `{kind, available, injecting}` |
| `last_poll_age_ms` | Age of the most recent attempt (success or failure) |
| `last_seen_age_ms`, `last_seen_unix_ms` | Age and wall-clock time of the last success |
| `stale` | No success within `2 × interval + timeout` (never-seen is stale) |
| `last_error` | Most recent transport/HTTP/parse error, with no token |

`/status` also reports `child_poll_interval_ms` and `child_poll_timeout_ms`
(`0`/configured timeout when disabled). A timeout or HTTP failure keeps the last
successful summary visible but sets `reachable:false`, leaves `stale` honest,
and records `last_error`; it never fabricates HLS/WebRTC/audio/power/input state.
The cache is pruned when a VM leaves the supervisor's tracked set.

### Metrics

The scrape surface is bounded by the hypervisor's VM set and deliberately does
not carry QMP socket paths, error strings, or sink names:

```text
ozma_supervisor_vms{state="discovered|tracked|running|failed|skipped|excluded"} N
ozma_supervisor_next_port{kind="hid_udp|http"} N
ozma_supervisor_vm_up{vm="<name>"} 0|1
ozma_supervisor_vm_restarts_total{vm="<name>"} N
ozma_supervisor_vm_port{vm="<name>",kind="hid_udp|http"} N
ozma_supervisor_vm_state{vm="<name>",state="<state>"} 1
ozma_supervisor_vm_child_up{vm="<name>"} 0|1
ozma_supervisor_vm_child_stale{vm="<name>"} 0|1
```

Child metrics appear only for VMs that have a poll record and carry only
reachability/staleness; child state strings, errors, sink names, and tokens stay
out of `/metrics`. The status documents never carry secrets or QMP output. QMP
socket paths remain in `/status` (as they were before child aggregation); use
`--status-port` network policy accordingly.

## Opt-in libvirt provisioning

Provisioning is **off by default**. Passing `--provision` allows the supervisor
to inspect the inactive libvirt XML with `virsh dumpxml --inactive <domain>` and,
when the domain has no QMP socket, define one extra `<qemu:commandline>` block
through `virsh define /dev/stdin`. The XML is never written to a file path, and
Proxmox configuration (`/var/run/qemu-server`) is never edited.

The block asks QEMU for three QMP chardevs and a D-Bus peer-to-peer display:

| Socket | Default path | Purpose |
|---|---|---|
| control | `/run/ozma/qmp/<domain>.sock` | capture, power, control QMP |
| input | `/run/ozma/qmp/<domain>-input.sock` | high-frequency input QMP |
| display | `/run/ozma/qmp/<domain>-display.sock` | D-Bus `add_client` setup |

It also adds `-display dbus,p2p=yes`. The directory is `--qmp-dir` when that
flag is set, otherwise `/run/ozma/qmp`.

Optional operator-supplied values:

* `--evdev-kbd <path>` **and** `--evdev-mouse <path>` add two QEMU
  `input-linux` objects (`ozma-kbd`, `ozma-mouse`). One path alone is rejected.
* `--provision-evdev` asks the supervisor to create those two devices itself
  from `/dev/uinput` and pass the real event paths. It conflicts with explicit
  `--evdev-kbd`/`--evdev-mouse` and requires `--provision`. See
  [Opt-in evdev device creation](#opt-in-evdev-device-creation).
* `--pulse-server <value>` adds a `PULSE_SERVER` `qemu:env` entry and replaces
  an exact `<audio id='1' type='none'/>` with `type='pulseaudio'`.
* `--xdg-runtime-dir <path>` adds an `XDG_RUNTIME_DIR` `qemu:env` entry.
* A successfully provisioned per-VM sink is injected as `PULSE_SINK` (and also
  replaces the `none` audio device). See
  [Opt-in per-VM audio sinks](#opt-in-per-vm-audio-sinks).

These flags only inject what actually exists. An explicit operator path is
used unchanged; a created device/sink is passed only after creation succeeded.
The supervisor does not create USB/media devices, grab host input devices, or
use the libvirt API.

### Opt-in evdev device creation

`--provision --provision-evdev` creates one virtual keyboard
(`ozma-kbd-<vm>`, vendor `0x1209`, product `0x0001`) and pointer
(`ozma-mouse-<vm>`, vendor `0x1209`, product `0x0002`) per VM with the Linux
`uinput` API. The devices stay open in the supervisor for the VM lifetime, so
QEMU's `input-linux` objects can open the real `/dev/input/eventN` paths.

* `--evdev-marker-dir <path>` (default `/run/ozma/evdev`) receives
  `<vm>.kbd` / `<vm>.mouse` files containing those event paths.
* A best-effort `ozma-kbd-<vm>` / `ozma-mouse-<vm>` symlink is created under
  `/dev/input/by-id`. Failure to create the symlink is logged; the marker and
  returned event path still refer to the real device.
* Devices are closed and markers/symlinks removed when the VM is removed or the
  supervisor shuts down.
* Creation needs a loaded `uinput` module and write access to `/dev/uinput`.
  If anything fails, the supervisor logs a warning, omits the `input-linux`
  block, and still provisions QMP/display normally. No path is fabricated.

### Opt-in per-VM audio sinks

`--provision-audio --audio-backend pipewire|pulseaudio` (default off) creates
one null sink per VM through `ozma-audio`'s `AudioRouter` (`pactl`, including
`pipewire-pulse` on PipeWire hosts):

* sink name `<prefix><vm>` with `--audio-sink-prefix` (default `ozma-`);
* description `Ozma-<vm>`, 2 channels, 48 kHz, monitor target
  `<sink>.monitor`;
* the sink is created before the per-VM `ozma-node` starts and its name is
  passed as `--audio-sink`;
* when libvirt XML is being provisioned, the same sink is injected as
  `PULSE_SINK` and the `none` audio device is switched to `pulseaudio`;
* on VM removal or supervisor shutdown, only sinks recorded as created by this
  tooling are unloaded. A pre-existing sink with the same name is reused and
  left alone;
* `--audio-track-root` is passed to the router as its reference-track root and
  requires `--provision-audio`;
* `--audio-state-file <path>` records ownership as JSON. The default is
  `$XDG_RUNTIME_DIR/ozma/audio-provision.json`, falling back to
  `/run/ozma/audio-provision.json` when `XDG_RUNTIME_DIR` is unset or empty.

The state file is versioned:
`{"version":1,"sinks":[{"vm":"<vm>","sink":"<sink>","created":true,"created_at":"<RFC 3339>"}]}`.
It is written atomically (temporary file in the same directory, then rename),
and the parent directory is created when needed. A missing, corrupt, or
unreadable file is logged and treated as empty rather than aborting startup.

Ownership survives supervisor restarts. A restarted supervisor reuses a sink it
recorded earlier and still unloads it on release. An existing sink that is not
recorded is reused but never claimed or unloaded. A recorded sink that has
already disappeared is cleared from the state on release. Persistence failures
are logged and do not fail VM startup.

`ozma-node` currently has `--audio-sink` but no `--audio-type` flag: a
non-empty sink is advertised as `audio_type=pipewire` even on the PulseAudio
backend. That is a known registration gap. Sink creation failure is non-fatal;
the node starts without `--audio-sink` and no sink is reported as ready.

The state records ownership by sink name, matching what `ozma-audio`'s router
exposes. If another process removes a recorded sink and later recreates the same
name, the record cannot distinguish the new object from the old one.

### Restart semantics

A newly defined XML does not create sockets until the VM starts. Restarting is a
second, explicit opt-in:

* **without `--provision-restart` (default):** the XML is defined, the
  supervisor logs `restart required`, does not spawn that VM's node, and waits.
  After the operator restarts the VM, discovery sees the real socket and starts
  the node. A configured-but-not-yet-created socket is not passed to a child.
* **with `--provision-restart`:** the supervisor runs `virsh destroy <domain>`
  (treating an already-stopped domain as success), `virsh start <domain>`, and
  waits up to 10 seconds for the control socket. It only returns success when
  the socket exists.

`--provision-restart` requires `--provision`. Neither flag is enabled by
`--no-auto-manage`.

> **Warning:** editing libvirt XML is opt-in and may require a VM restart. It can
> conflict with hand-maintained `qemu:commandline` blocks, and not every guest
> tolerates `-display dbus,p2p=yes`. If a domain already contains `ozma-mon`, the
> supervisor leaves its XML untouched.

### QMP input sockets

A dedicated QMP input socket is preferable for high-frequency input, but it is
not required. When the VM model exposes `qmp_input_path`, the supervisor passes
`--qmp-input-socket`; otherwise `ozma-qmp` uses the control socket in
single-socket mode. Both paths keep input on the guest QMP interface rather than
the hypervisor host's `uinput` devices.

## Still deferred

This is a safe core, not full VM device provisioning. The following are
deliberately **not** implemented here:

* using the libvirt API (only `virsh` subprocesses are used) and creating
  libvirt devices outside the `qemu:commandline` block;
* D-Bus framebuffer capture: the display socket is provisioned, but
  `ozma-capture` does not consume it yet;
* validating that QEMU actually opened the created evdev devices, that they are
  grabbed safely, or that input reaches the guest end to end;
* validating that a created sink is audible/non-silent, routing QEMU playback
  beyond `PULSE_SINK`, native PipeWire node creation, or a live supervisor
  restart against a real audio server/sink (ownership persistence itself is
  covered with a fake router and a temporary state file);
* USB/media, Looking Glass, and VBAN/UAC2 audio;
* per-child controls (reset/restart/stop) beyond the automatic crash-restart
  loop; the supervisor and controller views are read-only;
* controller multi-VM UI polish: the dashboard renders the read-only proxied
  document in Advanced/Amazing modes, but there is no dedicated VM page,
  history, alerting, or drill-down;
* child states beyond the connection summary (capture, detailed codec,
  per-session HID counters, QMP errors) and aggregate child metrics beyond
  reachability/staleness;
* a real libvirt/QEMU restart integration test (default tests use a fake
  command runner, fake evdev/audio provisioners, and XML fixtures; no test
  opens `/dev/uinput` or runs `pactl`);
* status and metrics aggregation.

Those remain separate Phase 2 workstreams in
`docs/rust-reconciliation-plan.md`.
