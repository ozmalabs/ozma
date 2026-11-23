# ozma-softnode

`ozma-softnode` is the hypervisor-side supervisor. It discovers QEMU/KVM VMs
through Proxmox, libvirt (`virsh`), or a QMP socket scan, and supervises one
real `ozma-node --profile vm` subprocess per VM that exposes a QMP socket.

This crate does **not** implement the per-VM runtime itself, and it never
spawns an `ozma-softnode` binary. The child binary defaults to `ozma-node` and
is configurable with `--node-bin`.

## What the supervisor does

For every discovered VM with a non-empty QMP socket:

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
added when the VM model exposes one; otherwise `ozma-qmp` uses the control
socket in single-socket mode. This keeps input on the guest QMP path instead of
the VM profile's host-side `uinput` default.

* starts new VMs, stops removed VMs, and leaves unchanged VMs alone;
* restarts a failed child with bounded exponential backoff
  (`--restart-backoff-ms`, default `1000`; `--restart-backoff-max-ms`, default
  `30000`); and
* stops every child and waits for it on SIGINT/SIGTERM.

A VM whose discovered `qmp_path` is empty is tracked for status/agent work but
is **not** started, and no socket path is fabricated. A warning is logged.

`--no-auto-manage` keeps discovery-only operation. `--no-auto-agent` keeps the
existing switch for in-guest agent provisioning.

### QMP input sockets

A dedicated QMP input socket is preferable for high-frequency input, but it is
not required. When the VM model exposes `qmp_input_path`, the supervisor passes
`--qmp-input-socket`; otherwise `ozma-qmp` uses the control socket in
single-socket mode. Both paths keep input on the guest QMP interface rather than
the hypervisor host's `uinput` devices.

## Still deferred

This is the subprocess transition, not full VM provisioning. The following are
deliberately **not** implemented here:

* creating the QMP socket or any display socket, and editing libvirt XML;
* D-Bus p2p display setup and QEMU `add_client` wiring;
* evdev `input-linux` device creation/attachment;
* PulseAudio/PipeWire per-VM sink routing and QEMU environment injection;
* libvirt power control, VM restart, USB/media, and Looking Glass;
* HLS/WebRTC surfaces in `ozma-node`.

Those remain separate Phase 2 workstreams in
`docs/rust-reconciliation-plan.md`.
