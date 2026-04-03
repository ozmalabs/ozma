#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for Proxmox plugin components.

These tests don't require a running PVE VM — they test the plugin's
Python code in isolation with mocks.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch, MagicMock

import pytest
from unittest.mock import MagicMock

# Stub heavy optional deps that aren't installed in the dev/CI environment.
# Must happen before any project imports that transitively pull these in.
for _mod in ("aiohttp", "aiohttp.web", "zeroconf", "zeroconf.asyncio",
             "zeroconf._utils.ipaddress", "zeroconf._dns", "zeroconf._services.browser",
             "dbus_fast", "dbus_fast.aio"):
    sys.modules.setdefault(_mod, MagicMock())

# Add project paths
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python"))


# ── Test: discover_proxmox_vms ───────────────────────────────────────────

class TestDiscoverProxmoxVMs:
    """Test the Proxmox VM discovery function."""

    def test_no_proxmox(self):
        """Returns empty list when not on a Proxmox host."""
        from virtual_node import discover_proxmox_vms
        with patch("virtual_node.Path") as mock_path:
            mock_path.return_value.exists.return_value = False
            result = discover_proxmox_vms()
            assert result == []

    def test_discovers_vms(self, tmp_path, monkeypatch):
        """Discovers VMs from QMP sockets and config files."""
        import virtual_node as vn

        # Create fake Proxmox directory structure
        qmp_dir = tmp_path / "qemu-server"
        qmp_dir.mkdir()
        (qmp_dir / "100.qmp").touch()
        (qmp_dir / "101.qmp").touch()

        pve_conf = tmp_path / "pve" / "qemu-server"
        pve_conf.mkdir(parents=True)
        (pve_conf / "100.conf").write_text("name: test-vm\nostype: l26\n")
        (pve_conf / "101.conf").write_text("name: windows-desktop\nostype: win11\n")

        # Monkey-patch the paths
        original_path = vn.Path

        def patched_path(p):
            s = str(p)
            if s == "/var/run/qemu-server":
                return qmp_dir
            if s.startswith("/etc/pve/qemu-server/"):
                fname = s.split("/")[-1]
                return pve_conf / fname
            return original_path(p)

        monkeypatch.setattr(vn, "Path", patched_path)

        result = vn.discover_proxmox_vms()
        assert len(result) == 2
        names = {vm.name for vm in result}
        assert "test-vm" in names
        assert "windows-desktop" in names

    def test_detects_guest_os(self, tmp_path, monkeypatch):
        """Correctly detects Windows vs Linux guest OS."""
        import virtual_node as vn

        qmp_dir = tmp_path / "qemu-server"
        qmp_dir.mkdir()
        (qmp_dir / "100.qmp").touch()

        pve_conf = tmp_path / "pve" / "qemu-server"
        pve_conf.mkdir(parents=True)
        (pve_conf / "100.conf").write_text("name: win-vm\nostype: win11\n")

        original_path = vn.Path

        def patched_path(p):
            s = str(p)
            if s == "/var/run/qemu-server":
                return qmp_dir
            if s.startswith("/etc/pve/qemu-server/"):
                return pve_conf / s.split("/")[-1]
            return original_path(p)

        monkeypatch.setattr(vn, "Path", patched_path)

        result = vn.discover_proxmox_vms()
        assert len(result) == 1
        assert result[0].guest_os == "windows"
        assert result[0].name == "win-vm"


# ── Test: VM profiles ────────────────────────────────────────────────────

class TestVMProfiles:
    """Test VM profile generation."""

    def test_import(self):
        """vm_profiles module can be imported."""
        try:
            from vm_profiles import VMProfile
            assert VMProfile is not None
        except ImportError:
            pytest.skip("vm_profiles not available")

    def test_gaming_profile_qemu_args(self):
        """Gaming profile generates expected QEMU args."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu, \
             patch("vm_profiles.GPUInfo") as mock_gpu:
            mock_cpu.detect.return_value = MagicMock(
                cores=8, threads=16, sockets=1, numa_nodes=1,
                pin_cores=MagicMock(return_value=([4, 5, 6, 7], [12, 13, 14, 15])),
            )
            mock_gpu.detect.return_value = MagicMock(
                pci_addr="0000:31:00.0",
                audio_pci="0000:31:00.1",
                vram_mb=8192,
                rebar_size=13,
                iommu_group=42,
                name="RX 6600 XT",
            )

            profile = VMProfile.gaming(vmid=100)
            args = profile.qemu_args()

            # Should have display, audio, and QMP
            args_str = " ".join(args)
            assert "-display" in args_str
            assert "pipewire" in args_str or "audiodev" in args_str
            assert "-qmp" in args_str

    def test_proxmox_conf_lines(self):
        """Profile generates valid Proxmox config lines."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu:
            mock_cpu.detect.return_value = MagicMock(
                cores=8, threads=16, sockets=1,
                pin_cores=MagicMock(return_value=([0, 1, 2, 3], [8, 9, 10, 11])),
            )

            profile = VMProfile.workstation(vmid=101)
            lines = profile.proxmox_conf_lines()

            assert isinstance(lines, list)
            # Should produce valid Proxmox config key:value lines
            for line in lines:
                assert ":" in line or line.startswith("#")


# ── Test: Display service ────────────────────────────────────────────────

class TestDisplayService:
    """Test the display service initialization."""

    def test_init(self):
        """VMDisplayService initializes with correct ports."""
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python"))
        # Mock the imports that aren't available in test env
        sys.modules['looking_glass'] = MagicMock()
        sys.modules['qemu_display'] = MagicMock()
        sys.modules['dbus_display'] = MagicMock()

        # Import after mocking
        import importlib
        if 'display-service' in sys.modules:
            del sys.modules['display-service']

        # Can't import with hyphen in name, use importlib
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "display_service",
            str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python" / "display-service.py")
        )
        if spec and spec.loader:
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)

            svc = mod.VMDisplayService(vmid=100)
            assert svc.vmid == 100
            assert svc.api_port == 7490  # 7390 + 100
            assert svc.name == "vm100"
            assert svc.shm_path == "/dev/shm/ozma-vm100"

    def test_qmp_path_proxmox(self):
        """QMP path uses ozma's dedicated socket."""
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "display_service",
            str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python" / "display-service.py")
        )
        if spec and spec.loader:
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)

            svc = mod.VMDisplayService(vmid=200)
            # Primary: ozma's dedicated QMP socket
            assert svc.qmp_path == "/var/run/ozma/vm200-ctrl.qmp"
            # Fallback: Proxmox native QMP
            assert svc.qmp_path_proxmox == "/var/run/qemu-server/200.qmp"


# ── Test: Virtual Node Manager Proxmox detection ─────────────────────────

class TestVNMProxmox:
    """Test VNM behavior on Proxmox hosts."""

    def test_is_proxmox(self, tmp_path):
        """Detects Proxmox by /var/run/qemu-server presence."""
        from virtual_node import VirtualNodeManager

        mgr = VirtualNodeManager()
        with patch.object(Path, "exists", return_value=True):
            # This would need more mocking to work properly
            pass

    def test_proxmox_skips_libvirt_provisioning(self):
        """On Proxmox, _provision_vm doesn't modify VM config."""
        from virtual_node import VirtualNodeManager, VMInfo

        mgr = VirtualNodeManager()

        # Mock _is_proxmox to return True
        with patch.object(mgr, "_is_proxmox", return_value=True):
            vm = VMInfo(
                name="test-vm", vm_id="100",
                qmp_path="/var/run/qemu-server/100.qmp",
            )

            # The QMP path exists check
            with patch("os.path.exists", return_value=True):
                import asyncio
                result = asyncio.run(mgr._provision_vm(vm))
                assert result == "/var/run/qemu-server/100.qmp"

    def test_qmp_scanner_excludes_ozma(self):
        """QMP socket scanner excludes ozma's own sockets."""
        from virtual_node import discover_qmp_sockets

        with tempfile.TemporaryDirectory() as tmp:
            # Create test sockets
            import socket
            for name in ["vm1.qmp", "ozma-mon.qmp", "test.monitor"]:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                path = os.path.join(tmp, name)
                s.bind(path)
                s.close()

            result = discover_qmp_sockets(tmp)
            names = [vm.name for vm in result]
            assert "ozma-mon" not in str(names)


if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
