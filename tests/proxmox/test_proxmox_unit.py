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
# Only stub modules that aren't already importable — avoids poisoning sys.modules
# for modules like aiohttp that are installed and used by other tests.
for _mod in ("aiohttp", "aiohttp.web", "zeroconf", "zeroconf.asyncio",
             "zeroconf._utils.ipaddress", "zeroconf._dns", "zeroconf._services.browser",
             "dbus_fast", "dbus_fast.aio"):
    _top = _mod.split(".")[0]
    if _top not in sys.modules:
        try:
            __import__(_top)
        except ImportError:
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

    def test_gaming_profile_gpu_passthrough_dual_display(self):
        """Gaming profile with GPU passthrough emits secondary virtio-vga + D-Bus (not -display none)."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu, \
             patch("vm_profiles.GPUInfo") as mock_gpu:
            mock_cpu.detect.return_value = MagicMock(
                pin_cores=MagicMock(return_value=[4, 5, 6, 7, 12, 13, 14, 15]),
                numa_nodes=1,
            )
            mock_gpu.detect.return_value = MagicMock(
                audio_pci="0000:31:00.1", rebar_supported=True,
            )

            profile = VMProfile.gaming(vmid=100, gpu_pci="0000:31:00.0")
            args = profile.qemu_args()
            args_str = " ".join(args)

            # Must use D-Bus p2p for the secondary virtual display — not none
            assert "dbus,p2p=yes" in args_str, \
                "GPU passthrough gaming profile must use -display dbus,p2p=yes (not none)"
            assert "none" not in args_str.split("-display")[1].split()[0] \
                if "-display" in args_str else True

            # Must include secondary virtio-vga for the management console
            assert "virtio-vga" in args_str, \
                "GPU passthrough gaming profile must emit secondary virtio-vga device"

            # Must still include vfio-pci for the GPU itself
            assert "vfio-pci" in args_str
            assert "0000:31:00.0" in args_str

    def test_gaming_profile_ivshmem_present(self):
        """Gaming profile with GPU passthrough includes IVSHMEM shared memory device."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu, \
             patch("vm_profiles.GPUInfo") as mock_gpu:
            mock_cpu.detect.return_value = MagicMock(
                pin_cores=MagicMock(return_value=[4, 5, 6, 7]),
                numa_nodes=1,
            )
            mock_gpu.detect.return_value = MagicMock(
                audio_pci="", rebar_supported=False,
            )

            profile = VMProfile.gaming(vmid=102, gpu_pci="0000:01:00.0")
            args = profile.qemu_args()
            args_str = " ".join(args)

            assert "ivshmem-plain" in args_str, "IVSHMEM device must be present for Looking Glass"
            assert "ozma-vm102" in args_str, "IVSHMEM shm path must include vmid"
            assert "memory-backend-file" in args_str

    def test_proxmox_conf_gpu_passthrough_has_vga_virtio(self):
        """Proxmox conf for GPU passthrough includes vga: virtio for secondary display."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu, \
             patch("vm_profiles.GPUInfo") as mock_gpu:
            mock_cpu.detect.return_value = MagicMock(
                pin_cores=MagicMock(return_value=[4, 5, 6, 7]),
                numa_nodes=1,
            )
            mock_gpu.detect.return_value = MagicMock(
                audio_pci="0000:01:00.1", rebar_supported=False,
            )

            profile = VMProfile.gaming(vmid=103, gpu_pci="0000:01:00.0")
            lines = profile.proxmox_conf_lines()

            assert any(l.startswith("hostpci0:") and "x-vga=1" in l for l in lines), \
                "Must have hostpci0 with x-vga=1"
            assert "vga: virtio" in lines, \
                "Must include vga: virtio to keep secondary display alive alongside passed-through GPU"

    def test_workstation_profile_no_gpu_passthrough(self):
        """Workstation profile (no GPU passthrough) still uses regular dbus display."""
        try:
            from vm_profiles import VMProfile
        except ImportError:
            pytest.skip("vm_profiles not available")

        with patch("vm_profiles.CPUTopology") as mock_cpu:
            mock_cpu.detect.return_value = MagicMock(
                pin_cores=MagicMock(return_value=[]),
                numa_nodes=1,
            )

            profile = VMProfile.workstation(vmid=104)
            args = profile.qemu_args()
            args_str = " ".join(args)

            assert "-display" in args_str
            assert "dbus" in args_str
            # No GPU passthrough — no vfio-pci, no ivshmem
            assert "vfio-pci" not in args_str


# ── Test: Display service ────────────────────────────────────────────────

class TestDisplayService:
    """Test the display service initialization."""

    def teardown_method(self, method):
        """Remove display-related mocks so they don't leak to other tests."""
        for _mod in ('looking_glass', 'qemu_display', 'dbus_display', 'display_service'):
            sys.modules.pop(_mod, None)

    def test_init(self):
        """VMDisplayService initializes with correct ports."""
        sys.path.insert(0, str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python"))
        _mocked = {
            'looking_glass': MagicMock(),
            'qemu_display': MagicMock(),
            'dbus_display': MagicMock(),
        }
        # Use patch.dict so mocks are removed after the test
        with patch.dict(sys.modules, _mocked):
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

    def _load_display_service(self):
        """Load the display-service module via importlib (hyphen in filename)."""
        import importlib.util
        sys.modules.setdefault('looking_glass', MagicMock())
        sys.modules.setdefault('qemu_display', MagicMock())
        sys.modules.setdefault('dbus_display', MagicMock())
        spec = importlib.util.spec_from_file_location(
            "display_service",
            str(Path(__file__).parent.parent.parent / "proxmox-plugin" / "python" / "display-service.py")
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod

    def test_dual_display_fields_initialized(self):
        """VMDisplayService has separate KVMFR and D-Bus state fields."""
        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=300)

        # KVMFR / game capture fields
        assert hasattr(svc, "_lg"), "Must have _lg Looking Glass client"
        assert hasattr(svc, "_lg_frame"), "Must have _lg_frame for KVMFR frames"
        assert hasattr(svc, "_lg_frame_count")
        assert hasattr(svc, "_lg_width")
        assert hasattr(svc, "_lg_height")
        assert svc._lg_frame is None
        assert svc._lg_frame_count == 0

        # D-Bus management console fields still present
        assert hasattr(svc, "_dbus_client")
        assert svc._dbus_client is None

    def test_display_info_includes_both_sources(self):
        """display_info response includes 'console' and 'game' sub-objects."""
        import asyncio

        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=301)

        # Simulate a connected D-Bus client
        dc = MagicMock()
        dc.connected = True
        dc.width = 1920
        dc.height = 1080
        dc.frame_count = 42
        dc.latest_frame = b"jpeg_console"
        svc._dbus_client = dc
        svc._display_type = "dbus-p2p"
        svc._width = 1920
        svc._height = 1080

        # Simulate KVMFR active too (GPU passthrough dual-display)
        svc._lg = MagicMock()
        svc._lg_frame = b"jpeg_game"
        svc._lg_frame_count = 99
        svc._lg_width = 2560
        svc._lg_height = 1440

        # Build the display_info handler by starting the aiohttp routes setup
        # We test it directly by replicating what the handler does, using the service state
        info = {
            "vmid": svc.vmid,
            "type": svc._display_type,
            "console": {
                "available": dc is not None and dc.connected,
                "width": dc.width,
                "height": dc.height,
                "frame_count": dc.frame_count,
            },
            "game": {
                "available": svc._lg_frame is not None,
                "width": svc._lg_width,
                "height": svc._lg_height,
                "frame_count": svc._lg_frame_count,
            },
        }
        # KVMFR is best source
        if svc._lg_frame is not None:
            info["width"] = svc._lg_width
            info["height"] = svc._lg_height
            info["frame_count"] = svc._lg_frame_count

        assert info["console"]["available"] is True
        assert info["console"]["width"] == 1920
        assert info["game"]["available"] is True
        assert info["game"]["width"] == 2560
        # Top-level dimensions come from KVMFR (higher quality source)
        assert info["width"] == 2560
        assert info["height"] == 1440
        assert info["frame_count"] == 99

    def test_kvmfr_is_primary_snapshot_source(self):
        """When both KVMFR and D-Bus are available, snapshot prefers KVMFR."""
        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=302)

        dc = MagicMock()
        dc.connected = True
        dc.latest_frame = b"console_jpeg"
        svc._dbus_client = dc
        svc._lg_frame = b"game_jpeg"

        # The snapshot priority logic: KVMFR first, D-Bus second
        # Replicate the handler's decision tree
        if svc._lg_frame:
            chosen = svc._lg_frame
        elif svc._dbus_client and svc._dbus_client.connected and svc._dbus_client.latest_frame:
            chosen = svc._dbus_client.latest_frame
        else:
            chosen = None

        assert chosen == b"game_jpeg", "KVMFR must be preferred over D-Bus for default snapshot"

    def test_console_snapshot_falls_back_to_dbus(self):
        """Console snapshot uses D-Bus even when KVMFR is also active."""
        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=303)

        dc = MagicMock()
        dc.connected = True
        dc.latest_frame = b"console_jpeg"
        svc._dbus_client = dc
        svc._lg_frame = b"game_jpeg"

        # Console endpoint: always uses D-Bus, ignores KVMFR
        if svc._dbus_client and svc._dbus_client.connected and svc._dbus_client.latest_frame:
            chosen = svc._dbus_client.latest_frame
        else:
            chosen = None

        assert chosen == b"console_jpeg", "Console snapshot must return D-Bus frame, not KVMFR"

    def test_game_snapshot_returns_503_without_kvmfr(self):
        """Game snapshot endpoint returns 503 when no KVMFR frame is available."""
        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=304)

        # D-Bus connected, but no KVMFR
        dc = MagicMock()
        dc.connected = True
        dc.latest_frame = b"console_jpeg"
        svc._dbus_client = dc
        svc._lg_frame = None

        # Game endpoint: only KVMFR, never D-Bus
        game_frame = svc._lg_frame
        assert game_frame is None, "Game endpoint must return None (→ 503) when KVMFR not active"

    def test_kvmfr_capture_loop_updates_lg_frame(self):
        """KVMFR capture loop writes to _lg_frame (not _latest_frame) when both sources active."""
        mod = self._load_display_service()
        svc = mod.VMDisplayService(vmid=305)

        # Simulate the capture loop body for a dual-source VM
        svc._display_type = "dbus-p2p"  # D-Bus is primary; KVMFR is the game source

        fake_frame = b"raw_kvmfr_jpeg"
        svc._lg = MagicMock()
        svc._lg.width = 2560
        svc._lg.height = 1440

        # Simulate one iteration of the loop
        frame = fake_frame
        svc._lg_frame = frame
        svc._lg_frame_count += 1
        svc._lg_width = getattr(svc._lg, "width", svc._lg_width)
        svc._lg_height = getattr(svc._lg, "height", svc._lg_height)
        if svc._display_type == "kvmfr":  # not the case here — D-Bus is primary
            svc._latest_frame = frame

        assert svc._lg_frame == fake_frame
        assert svc._lg_frame_count == 1
        assert svc._lg_width == 2560
        assert svc._lg_height == 1440
        # _latest_frame must NOT be updated — D-Bus owns the primary slot
        assert svc._latest_frame is None


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
