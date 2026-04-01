# Build the KVMFR QEMU display backend
#
# This needs to be built inside the QEMU source tree. The Makefile
# provides convenience targets for the development workflow.
#
# Quick start:
#   make setup QEMU_SRC=~/qemu    # symlink into QEMU tree
#   make build QEMU_SRC=~/qemu    # build QEMU with kvmfr backend
#
# Or manually:
#   cp kvmfr.c ~/qemu/ui/
#   edit ~/qemu/ui/meson.build to add kvmfr
#   cd ~/qemu/build && ninja

QEMU_SRC ?= $(HOME)/qemu
QEMU_BUILD ?= $(QEMU_SRC)/build

.PHONY: setup build clean check

setup:
	@echo "Linking kvmfr.c into QEMU source tree..."
	ln -sf $(CURDIR)/kvmfr.c $(QEMU_SRC)/ui/kvmfr.c
	@echo ""
	@echo "Now add to $(QEMU_SRC)/ui/meson.build:"
	@echo ""
	@echo "  kvmfr_ss = ss.source_set()"
	@echo "  kvmfr_ss.add(when: [pixman], if_true: files('kvmfr.c'))"
	@echo "  ui_modules += {'kvmfr' : kvmfr_ss}"
	@echo ""
	@echo "Then: cd $(QEMU_SRC) && mkdir -p build && cd build && ../configure && ninja"

build:
	cd $(QEMU_BUILD) && ninja

clean:
	rm -f $(QEMU_SRC)/ui/kvmfr.c

check:
	@echo "Checking if KVMFR SHM exists..."
	@test -f /dev/shm/looking-glass && echo "OK: /dev/shm/looking-glass exists" || echo "MISSING: create with: truncate -s 32M /dev/shm/looking-glass"
	@echo ""
	@echo "Checking for QEMU with kvmfr module..."
	@ls $(QEMU_BUILD)/ui-kvmfr.so 2>/dev/null && echo "OK: module built" || echo "MISSING: run 'make build'"
