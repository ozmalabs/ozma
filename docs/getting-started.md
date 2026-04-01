# Getting Started

This guide walks you through running the full Ozma stack using the dev harness — no hardware required.

## Prerequisites

```bash
cd dev
make deps          # checks: qemu-system-riscv64, qemu-system-x86_64, usbip, python3
make ssh-key       # generate SSH key for VM access
make build-node-image   # build Alpine RISC-V disk image (requires sudo, ~5 min)
```

You'll also need Python 3.11+ with pip.

## Install controller dependencies

```bash
cd controller
pip install -r requirements.txt
```

## Quick start (three terminals)

### Terminal 1: Start the controller

```bash
python3 controller/main.py --virtual-only
```

The `--virtual-only` flag skips evdev keyboard grab so you keep normal input.

The controller starts on port 7380. Open http://localhost:7380 for the web UI.

### Terminal 2: Start VMs and soft nodes

```bash
bash demo/start_vms.sh
```

This launches two QEMU x86_64 VMs (`vm1` and `vm2`) and a soft node process for each. The soft nodes register with the controller via mDNS.

### Terminal 3: (Optional) Start the RISC-V hardware node

```bash
cd dev && make node-vm
```

This boots a RISC-V VM running the real node daemon with USB gadget emulation via `dummy_hcd`.

## Verify it's working

```bash
# List registered nodes
curl http://localhost:7380/api/v1/nodes | python3 -m json.tool

# Switch to vm1
curl -X POST http://localhost:7380/api/v1/scenarios/vm1/activate

# Switch to vm2
curl -X POST http://localhost:7380/api/v1/scenarios/vm2/activate
```

## Run E2E tests

With the controller and VMs running:

```bash
python3 tests/test_e2e_switching.py
```

## Stop everything

```bash
bash demo/start_vms.sh stop    # stop VMs and soft nodes
# Ctrl+C the controller
```

## Log locations

| Log | Path |
|-----|------|
| Controller | `/tmp/ozma-controller.log` |
| vm1 soft node | `/tmp/ozma-softnode-vm1.log` |
| vm2 soft node | `/tmp/ozma-softnode-vm2.log` |
| vm1 QEMU serial | `demo/logs/vm1.log` |
| vm2 QEMU serial | `demo/logs/vm2.log` |

## Next steps

- Read the [Architecture](architecture.md) doc for system design
- Read the [Protocols](protocols.md) doc for wire formats
- Browse the REST API at `http://localhost:7380/docs` (FastAPI auto-generated)
- Try connecting a physical node — see `tinynode/README.md` for SBC setup
