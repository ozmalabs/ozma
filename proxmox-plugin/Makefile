PACKAGE = ozma-proxmox-plugin
VERSION = 1.0.0
QEMU_SRC ?= $(HOME)/qemu

.PHONY: all deb clean install kvmfr test

all: deb

# Build the .deb package
deb:
	dpkg-buildpackage -us -uc -b

# Build just the QEMU kvmfr display module
kvmfr:
	$(MAKE) -C ../dev/qemu-display setup QEMU_SRC=$(QEMU_SRC)
	cd $(QEMU_SRC)/build && ninja ui/ui-kvmfr.so
	cp $(QEMU_SRC)/build/ui/ui-kvmfr.so qemu/

# Install directly (without building .deb)
install:
	# Perl modules → Proxmox plugin path
	install -D -m 644 perl/Ozma.pm $(DESTDIR)/usr/share/perl5/PVE/API2/Ozma.pm
	install -D -m 644 perl/OzmaQemu.pm $(DESTDIR)/usr/share/perl5/PVE/QemuServer/Ozma.pm

	# Python services (plugin-specific)
	install -d $(DESTDIR)/usr/lib/ozma-proxmox
	install -m 755 python/*.py $(DESTDIR)/usr/lib/ozma-proxmox/

	# Bundled soft node (the complete ozma softnode)
	install -d $(DESTDIR)/usr/lib/ozma-proxmox/softnode
	install -m 644 ../softnode/*.py $(DESTDIR)/usr/lib/ozma-proxmox/softnode/

	# Bundled agent source (for offline install into VMs)
	install -d $(DESTDIR)/usr/lib/ozma-proxmox/agent
	install -m 644 ../agent/*.py $(DESTDIR)/usr/lib/ozma-proxmox/agent/

	# QEMU display module
	install -D -m 644 qemu/ui-kvmfr.so $(DESTDIR)/usr/lib/x86_64-linux-gnu/qemu/ui-kvmfr.so 2>/dev/null || true

	# Systemd services
	install -D -m 644 debian/ozma-display@.service $(DESTDIR)/lib/systemd/system/ozma-display@.service
	install -D -m 644 debian/ozma-proxmox.service $(DESTDIR)/lib/systemd/system/ozma-proxmox.service 2>/dev/null || true

	# Hooks
	install -D -m 755 hooks/ozma-vm-hook $(DESTDIR)/usr/lib/ozma-proxmox/ozma-vm-hook

	# Web UI
	install -d $(DESTDIR)/usr/share/javascript/proxmox-widget-toolkit/ozma
	install -m 644 ui/*.js $(DESTDIR)/usr/share/javascript/proxmox-widget-toolkit/ozma/

	# Config
	install -D -m 644 etc/proxmox-plugin.conf $(DESTDIR)/etc/ozma/proxmox-plugin.conf

	# State directories
	install -d $(DESTDIR)/var/lib/ozma
	install -d $(DESTDIR)/var/run/ozma

# Test the profile generator
test:
	python3 python/apply-profile.py --profile gaming --vmid 100 --gpu 0000:29:00.0 --dry-run
	python3 python/apply-profile.py --profile workstation --vmid 101 --dry-run
	python3 python/apply-profile.py --profile server --vmid 102 --dry-run
	python3 python/apply-profile.py --profile media --vmid 103 --dry-run
	@echo "All profiles OK"

clean:
	rm -rf debian/.debhelper debian/ozma-proxmox-plugin debian/files
	rm -f debian/ozma-proxmox-plugin.*.debhelper
	rm -f debian/ozma-proxmox-plugin.substvars
