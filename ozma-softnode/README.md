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
  `input-linux` objects (`ozma-kbd`, `ozma-mouse`). One path alone is rejected;
  the supervisor does not create or claim evdev devices.
* `--pulse-server <value>` adds a `PULSE_SERVER` `qemu:env` entry and replaces
  an exact `<audio id='1' type='none'/>` with `type='pulseaudio'`.
* `--xdg-runtime-dir <path>` adds an `XDG_RUNTIME_DIR` `qemu:env` entry.

These flags only inject what the operator configured. The supervisor does not
create PulseAudio/PipeWire sinks, evdev devices, USB/media devices, or audio
routes.

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
* creating/attaching evdev `input-linux` devices, and grabbing them safely;
* per-VM PulseAudio/PipeWire sink routing or verifying that audio is non-silent;
* USB/media, Looking Glass, and audio routing;
* a real libvirt/QEMU restart integration test (default tests use a fake
  command runner and XML fixtures);
* status and metrics aggregation.

Those remain separate Phase 2 workstreams in
`docs/rust-reconciliation-plan.md`.
