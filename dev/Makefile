# Ozma dev harness — emulated RISC-V node + target VM
#
# Quick start:
#   make deps          -- verify host tools
#   make images        -- download QEMU disk images
#   make ssh-key       -- generate SSH key for VM access
#   make node-vm       -- start RISC-V QEMU VM (background)
#   make provision-node -- first-time: install deps in node VM
#   make target-vm     -- start Alpine x86_64 target VM (background)
#   make target-install -- first-time: run Alpine setup
#   make provision-target -- first-time: install usbip on target
#   make init-node     -- (re)start ozma node.py inside node VM
#   make feed-video    -- push test pattern into v4l2loopback
#   make attach-usb    -- USB/IP: connect gadget to target VM
#   make logs          -- tail node.py logs
#   make status        -- show VM status
#   make stop          -- stop all VMs
#
# TAP networking (enables mDNS, requires sudo):
#   sudo make tap-up   -- create tap interface
#   sudo make tap-down -- remove tap interface

SHELL     := /bin/bash
DEVDIR    := $(dir $(realpath $(firstword $(MAKEFILE_LIST))))
IMAGESDIR := $(DEVDIR)../images
SSHKEY    := $(IMAGESDIR)/dev_key
SSH       := ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
SCP       := scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR

include $(DEVDIR)config.env
export

.PHONY: deps images ssh-key \
        build-node-image \
        node-vm provision-node init-node feed-video \
        connect-vms disconnect-vms \
        target-vm target-install provision-target attach-usb \
        receiver-vm \
        tap-up tap-down \
        logs logs-target logs-receiver shell-node shell-target \
        status stop clean

# ── Prerequisites ─────────────────────────────────────────────────────────────

deps:
	@bash $(DEVDIR)scripts/check-deps.sh

$(IMAGESDIR):
	@mkdir -p $(IMAGESDIR)

ssh-key: $(IMAGESDIR)
	@if [[ ! -f "$(SSHKEY)" ]]; then \
	    ssh-keygen -t ed25519 -f "$(SSHKEY)" -N "" -C "ozma-dev"; \
	    echo "SSH key generated: $(SSHKEY)"; \
	else \
	    echo "SSH key already exists: $(SSHKEY)"; \
	fi

images: $(IMAGESDIR) ssh-key
	@bash $(DEVDIR)scripts/fetch-images.sh

# Build a fully-configured Alpine RISC-V disk image (needs root for qemu-nbd)
build-node-image: $(IMAGESDIR) ssh-key
	@echo "Building RISC-V node image (requires sudo)..."
	@sudo bash $(DEVDIR)scripts/build-riscv-image.sh

# ── RISC-V node VM ────────────────────────────────────────────────────────────

node-vm:
	@bash $(DEVDIR)riscv-node/launch.sh

provision-node:
	@SSH_KEY=$(SSHKEY) bash $(DEVDIR)riscv-node/provision.sh

# SSH into node VM and run init.sh (reloads modules + restarts node.py)
init-node:
	@echo "=== Initialising ozma node inside VM ==="
	@$(SSH) -i $(SSHKEY) -p $(NODE_SSH_PORT) $(NODE_USER)@localhost \
	    'bash /home/$(NODE_USER)/ozma-init.sh'

sync-node:
	@echo "=== Syncing node package to VM ==="
	@$(SCP) -i $(SSHKEY) -P $(NODE_SSH_PORT) -r \
	    $(DEVDIR)../node $(NODE_USER)@localhost:/home/$(NODE_USER)/ozma-node/
	@$(SCP) -i $(SSHKEY) -P $(NODE_SSH_PORT) -r \
	    $(DEVDIR)../tinynode/gadget $(NODE_USER)@localhost:/home/$(NODE_USER)/ozma-node/gadget/
	@echo "Sync done. Run: make init-node"

feed-video:
	@VIDEO=$(VIDEO) SSH_KEY=$(SSHKEY) bash $(DEVDIR)scripts/feed-video.sh

# ── Target VM ─────────────────────────────────────────────────────────────────

target-vm:
	@bash $(DEVDIR)target/launch.sh

# First-time Alpine install (interactive: run setup-alpine, then shutdown)
target-install:
	@echo "=== Booting Alpine ISO for first-time install ==="
	@echo "  Inside the VM: setup-alpine"
	@echo "  When prompted for disk: /dev/vda, sys mode"
	@echo "  Set root password; enable sshd; then shutdown."
	@echo "  Then: make target-vm  (boots from disk next time)"
	@bash $(DEVDIR)target/launch.sh

provision-target:
	@SSH_KEY=$(SSHKEY) bash $(DEVDIR)target/provision.sh

attach-usb:
	@SSH_KEY=$(SSHKEY) bash $(DEVDIR)target/attach-usb.sh

# Connect RISC-V node USB gadget → host vhci → vm1 (or --target vm2)
connect-vms:
	@bash $(DEVDIR)riscv-node/connect-to-vms.sh $(ARGS)

disconnect-vms:
	@bash $(DEVDIR)riscv-node/connect-to-vms.sh --detach

# ── Receiver VM (operator console) ───────────────────────────────────────────

receiver-vm:
	@bash $(DEVDIR)receiver/launch.sh

logs-receiver:
	@echo "=== Receiver VM serial log ==="
	@tail -f $(DEVDIR)../demo/logs/receiver.log

# ── TAP networking (needs root) ───────────────────────────────────────────────

tap-up:
	@bash $(DEVDIR)scripts/setup-tap.sh up

tap-down:
	@bash $(DEVDIR)scripts/setup-tap.sh down

# ── Operations ────────────────────────────────────────────────────────────────

logs:
	@echo "=== Node VM: node.py log ==="
	@$(SSH) -i $(SSHKEY) -p $(NODE_SSH_PORT) $(NODE_USER)@localhost \
	    'tail -f /var/log/ozma-node.log'

logs-target:
	@echo "=== Target VM serial log ==="
	@tail -f $(IMAGESDIR)/target-vm.log

shell-node:
	@$(SSH) -i $(SSHKEY) -p $(NODE_SSH_PORT) $(NODE_USER)@localhost

shell-target:
	@$(SSH) -i $(SSHKEY) -p $(TARGET_SSH_PORT) $(TARGET_USER)@localhost

status:
	@echo "=== Node VM ==="
	@if [[ -f $(IMAGESDIR)/node-vm.pid ]] && kill -0 $$(cat $(IMAGESDIR)/node-vm.pid) 2>/dev/null; then \
	    echo "  Running (pid $$(cat $(IMAGESDIR)/node-vm.pid))"; \
	    $(SSH) -i $(SSHKEY) -p $(NODE_SSH_PORT) $(NODE_USER)@localhost \
	        'ps aux | grep -E "node.py|usbipd|ffmpeg" | grep -v grep' 2>/dev/null || echo "  (not yet booted)"; \
	else \
	    echo "  Stopped"; \
	fi
	@echo ""
	@echo "=== Target VM ==="
	@if [[ -f $(IMAGESDIR)/target-vm.pid ]] && kill -0 $$(cat $(IMAGESDIR)/target-vm.pid) 2>/dev/null; then \
	    echo "  Running (pid $$(cat $(IMAGESDIR)/target-vm.pid))"; \
	else \
	    echo "  Stopped"; \
	fi

stop:
	@echo "=== Stopping VMs ==="
	@if [[ -f $(IMAGESDIR)/node-vm.pid ]]; then \
	    kill $$(cat $(IMAGESDIR)/node-vm.pid) 2>/dev/null || true; \
	    rm -f $(IMAGESDIR)/node-vm.pid; \
	    echo "  Node VM stopped."; \
	fi
	@if [[ -f $(IMAGESDIR)/target-vm.pid ]]; then \
	    kill $$(cat $(IMAGESDIR)/target-vm.pid) 2>/dev/null || true; \
	    rm -f $(IMAGESDIR)/target-vm.pid; \
	    echo "  Target VM stopped."; \
	fi
	@bash $(DEVDIR)receiver/launch.sh stop 2>/dev/null || true

clean: stop
	@echo "Remove disk images? [y/N] " && read ans && [[ "$$ans" == "y" ]] && \
	    rm -f $(IMAGESDIR)/*.qcow2 $(IMAGESDIR)/*.img $(IMAGESDIR)/*.iso && \
	    echo "Images removed." || echo "Aborted."
