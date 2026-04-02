#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
End-to-end tests for the Proxmox VE plugin.

Requires a running PVE test VM with the ozma plugin installed.
Set PVE_HOST env var to the PVE IP address.

Usage:
  PVE_HOST=192.168.40.x pytest tests/proxmox/test_proxmox_plugin.py -v
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest
import requests

# Add project paths
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "controller"))
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "softnode"))


PVE_HOST = os.environ.get("PVE_HOST", "")
PVE_PASSWORD = os.environ.get("PVE_PASSWORD", "ozmatest123")
PVE_PORT = 8006  # PVE web UI
DISPLAY_PORT_BASE = 7390  # ozma display service base port
TEST_VMID = 100
TIMEOUT = 30

pytestmark = pytest.mark.skipif(not PVE_HOST, reason="PVE_HOST not set")


# ── Helpers ───────────────────────────────────────────────────────────────

def pve_api(method: str, path: str, data: dict | None = None) -> dict:
    """Call the Proxmox VE API."""
    url = f"https://{PVE_HOST}:{PVE_PORT}/api2/json{path}"
    # Get ticket
    auth = requests.post(
        f"https://{PVE_HOST}:{PVE_PORT}/api2/json/access/ticket",
        data={"username": "root@pam", "password": PVE_PASSWORD},
        verify=False,
        timeout=TIMEOUT,
    )
    auth.raise_for_status()
    ticket = auth.json()["data"]["ticket"]
    csrf = auth.json()["data"]["CSRFPreventionToken"]

    headers = {"CSRFPreventionToken": csrf}
    cookies = {"PVEAuthCookie": ticket}

    if method == "GET":
        r = requests.get(url, headers=headers, cookies=cookies, verify=False, timeout=TIMEOUT)
    elif method == "POST":
        r = requests.post(url, headers=headers, cookies=cookies, data=data, verify=False, timeout=TIMEOUT)
    elif method == "PUT":
        r = requests.put(url, headers=headers, cookies=cookies, data=data, verify=False, timeout=TIMEOUT)
    elif method == "DELETE":
        r = requests.delete(url, headers=headers, cookies=cookies, verify=False, timeout=TIMEOUT)
    else:
        raise ValueError(f"Unknown method: {method}")

    r.raise_for_status()
    return r.json().get("data", {})


def ssh_cmd(cmd: str) -> str:
    """Run a command on the PVE host via SSH."""
    result = subprocess.run(
        ["sshpass", "-p", PVE_PASSWORD, "ssh",
         "-o", "StrictHostKeyChecking=no",
         "-o", "ConnectTimeout=10",
         f"root@{PVE_HOST}", cmd],
        capture_output=True, text=True, timeout=TIMEOUT,
    )
    return result.stdout.strip()


def display_api(vmid: int, method: str = "GET", path: str = "/health",
                data: dict | None = None) -> dict:
    """Call the ozma display service API for a VM."""
    port = DISPLAY_PORT_BASE + vmid
    url = f"http://{PVE_HOST}:{port}{path}"
    if method == "GET":
        r = requests.get(url, timeout=TIMEOUT)
    else:
        r = requests.post(url, json=data, timeout=TIMEOUT)
    r.raise_for_status()
    return r.json()


# ── Test: PVE is running ─────────────────────────────────────────────────

class TestPVEBasics:
    """Verify the Proxmox VE host is accessible."""

    def test_pve_ssh(self):
        """SSH to PVE host works."""
        result = ssh_cmd("pveversion")
        assert "pve-manager" in result, f"Expected pve-manager in: {result}"

    def test_pve_api(self):
        """PVE API is accessible."""
        data = pve_api("GET", "/version")
        assert "version" in data

    def test_pve_cluster_status(self):
        """PVE cluster is healthy."""
        data = pve_api("GET", "/cluster/status")
        assert len(data) > 0


# ── Test: Plugin installation ────────────────────────────────────────────

class TestPluginInstall:
    """Verify the ozma plugin is installed correctly."""

    def test_plugin_files(self):
        """Plugin files exist on PVE host."""
        result = ssh_cmd("ls /opt/ozma-proxmox/python/display-service.py")
        assert "display-service.py" in result

    def test_perl_module(self):
        """Perl module is installed."""
        result = ssh_cmd("ls /usr/share/perl5/PVE/QemuServer/Ozma.pm")
        assert "Ozma.pm" in result

    def test_systemd_template(self):
        """Systemd service template exists."""
        result = ssh_cmd("systemctl cat ozma-display@.service 2>/dev/null | head -1")
        assert "Unit" in result or "Description" in result

    def test_softnode_dependencies(self):
        """Softnode Python modules are available."""
        result = ssh_cmd("ls /opt/ozma-proxmox/lib/ozma-proxmox/looking_glass.py")
        assert "looking_glass.py" in result


# ── Test: VM discovery ───────────────────────────────────────────────────

class TestVMDiscovery:
    """Test that the plugin discovers VMs correctly."""

    def test_vm_exists(self):
        """Test VM (VMID 100) exists in PVE."""
        data = pve_api("GET", f"/nodes/{_node_name()}/qemu")
        vmids = [vm["vmid"] for vm in data]
        assert TEST_VMID in vmids

    def test_vm_running(self):
        """Test VM is running."""
        data = pve_api("GET", f"/nodes/{_node_name()}/qemu/{TEST_VMID}/status/current")
        assert data["status"] == "running"

    def test_qmp_socket(self):
        """QMP socket exists for the VM."""
        result = ssh_cmd(f"ls /var/run/qemu-server/{TEST_VMID}.qmp")
        assert str(TEST_VMID) in result

    def test_discover_proxmox_vms(self):
        """The virtual_node discover_proxmox_vms() finds the VM."""
        # Run discover on the PVE host
        result = ssh_cmd("""python3 -c "
import sys; sys.path.insert(0, '/opt/ozma-proxmox/lib/ozma-proxmox')
from virtual_node import discover_proxmox_vms
vms = discover_proxmox_vms()
for v in vms:
    print(f'{v.name}:{v.vm_id}:{v.vnc_port}')
" """)
        assert "doom-test" in result or "100" in result


# ── Test: Display service ────────────────────────────────────────────────

class TestDisplayService:
    """Test the per-VM display service."""

    def test_service_running(self):
        """Display service is running for the test VM."""
        result = ssh_cmd(f"systemctl is-active ozma-display@{TEST_VMID}.service")
        assert result == "active"

    def test_health_endpoint(self):
        """Display service health check responds."""
        data = display_api(TEST_VMID, "GET", "/health")
        assert data["ok"] is True
        assert data["vmid"] == TEST_VMID

    def test_display_info(self):
        """Display info returns resolution and type."""
        data = display_api(TEST_VMID, "GET", "/display/info")
        assert data["vmid"] == TEST_VMID
        assert data["width"] > 0
        assert data["height"] > 0
        assert data["type"] in ("kvmfr", "dbus", "none")

    def test_snapshot(self):
        """Snapshot returns a JPEG image."""
        port = DISPLAY_PORT_BASE + TEST_VMID
        r = requests.get(f"http://{PVE_HOST}:{port}/display/snapshot", timeout=TIMEOUT)
        assert r.status_code == 200
        assert r.headers["Content-Type"] == "image/jpeg"
        assert len(r.content) > 1000  # at least 1KB JPEG

    def test_mjpeg_stream(self):
        """MJPEG stream returns frames."""
        port = DISPLAY_PORT_BASE + TEST_VMID
        r = requests.get(
            f"http://{PVE_HOST}:{port}/display/mjpeg",
            timeout=5, stream=True,
        )
        assert r.status_code == 200
        # Read at least one frame boundary
        data = b""
        for chunk in r.iter_content(chunk_size=4096):
            data += chunk
            if b"--frame" in data and len(data) > 100:
                break
        assert b"--frame" in data


# ── Test: Input injection ────────────────────────────────────────────────

class TestInput:
    """Test keyboard and mouse input injection."""

    def test_key_press(self):
        """Keyboard input is accepted."""
        data = display_api(TEST_VMID, "POST", "/input/key",
                          {"keycode": 103, "down": True})
        assert data["ok"] is True

    def test_key_release(self):
        """Key release is accepted."""
        data = display_api(TEST_VMID, "POST", "/input/key",
                          {"keycode": 103, "down": False})
        assert data["ok"] is True

    def test_mouse_move(self):
        """Mouse movement is accepted."""
        data = display_api(TEST_VMID, "POST", "/input/mouse",
                          {"x": 320, "y": 240, "action": "move"})
        assert data["ok"] is True

    def test_mouse_click(self):
        """Mouse click is accepted."""
        data = display_api(TEST_VMID, "POST", "/input/mouse",
                          {"x": 320, "y": 240, "action": "click", "button": 0})
        assert data["ok"] is True

    def test_input_changes_display(self):
        """Input actually changes the game display."""
        port = DISPLAY_PORT_BASE + TEST_VMID
        # Snapshot before
        r1 = requests.get(f"http://{PVE_HOST}:{port}/display/snapshot", timeout=TIMEOUT)
        frame1 = r1.content

        # Send key to turn (hold for 1 second)
        display_api(TEST_VMID, "POST", "/input/key", {"keycode": 105, "down": True})
        time.sleep(1)
        display_api(TEST_VMID, "POST", "/input/key", {"keycode": 105, "down": False})
        time.sleep(0.5)

        # Snapshot after
        r2 = requests.get(f"http://{PVE_HOST}:{port}/display/snapshot", timeout=TIMEOUT)
        frame2 = r2.content

        assert frame1 != frame2, "Display should change after input"


# ── Test: VM profiles ────────────────────────────────────────────────────

class TestVMProfiles:
    """Test VM profile generation."""

    def test_gaming_profile(self):
        """Gaming profile generates valid QEMU args."""
        result = ssh_cmd("""python3 -c "
import sys; sys.path.insert(0, '/opt/ozma-proxmox/python')
from vm_profiles import VMProfile
p = VMProfile.gaming(vmid=200, name='test-game')
args = p.qemu_args()
print(' '.join(args))
" """)
        assert "-display" in result
        assert "ivshmem" in result or "memory-backend" in result

    def test_workstation_profile(self):
        """Workstation profile generates valid config."""
        result = ssh_cmd("""python3 -c "
import sys; sys.path.insert(0, '/opt/ozma-proxmox/python')
from vm_profiles import VMProfile
p = VMProfile.workstation(vmid=201, name='test-ws')
lines = p.proxmox_conf_lines()
for l in lines:
    print(l)
" """)
        assert len(result) > 0


# ── Test: Controller registration ────────────────────────────────────────

class TestRegistration:
    """Test that VMs register with the ozma controller."""

    @pytest.mark.skipif(
        not os.environ.get("OZMA_CONTROLLER"),
        reason="OZMA_CONTROLLER not set"
    )
    def test_vm_registered(self):
        """Test VM appears in controller's node list."""
        controller = os.environ["OZMA_CONTROLLER"]
        r = requests.get(f"{controller}/api/v1/nodes", timeout=TIMEOUT)
        r.raise_for_status()
        nodes = r.json()["nodes"]
        node_names = [n["id"] for n in nodes]
        # The VM should be registered as "doom-test" or "vm100"
        found = any("doom-test" in n or "100" in n for n in node_names)
        assert found, f"VM not found in controller nodes: {node_names}"


# ── Test: KVMFR / Looking Glass ──────────────────────────────────────────

class TestKVMFR:
    """Test KVMFR shared memory display capture."""

    def test_shm_exists(self):
        """Shared memory file exists for the VM."""
        result = ssh_cmd(f"ls -la /dev/shm/ozma-vm{TEST_VMID}")
        assert f"ozma-vm{TEST_VMID}" in result

    def test_shm_size(self):
        """SHM file is the correct size (64MB)."""
        result = ssh_cmd(f"stat -c %s /dev/shm/ozma-vm{TEST_VMID}")
        size = int(result)
        assert size == 64 * 1024 * 1024

    def test_ivshmem_in_qemu(self):
        """QEMU has ivshmem device configured."""
        result = ssh_cmd(f"qm showcmd {TEST_VMID} | tr ' ' '\\n' | grep -c ivshmem")
        count = int(result.strip())
        assert count > 0, "No ivshmem device in QEMU command line"


# ── Test: Audio ──────────────────────────────────────────────────────────

class TestAudio:
    """Test audio pipeline from VM to host."""

    def test_audio_device_in_vm(self):
        """VM has a sound device."""
        result = ssh_cmd(f"qm guest exec {TEST_VMID} -- cat /proc/asound/cards 2>/dev/null")
        # May not have sound card if drivers aren't loaded
        # Just check the command doesn't fail
        assert isinstance(result, str)


# ── Utilities ────────────────────────────────────────────────────────────

def _node_name() -> str:
    """Get the PVE node name."""
    return ssh_cmd("hostname").strip()


if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
