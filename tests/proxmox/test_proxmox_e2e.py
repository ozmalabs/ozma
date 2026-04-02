#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
End-to-end Proxmox plugin tests — runs against a real PVE instance.

Connects to a PVE test VM via SSH and runs tests directly inside it.
The PVE VM must have:
  - A running VM (VMID 100) with ozma QEMU args configured
  - The ozma Proxmox plugin installed at /usr/lib/ozma-proxmox/
  - The Perl module at /usr/share/perl5/PVE/QemuServer/Ozma.pm

Setup:
  1. Build PVE ISO:  bash tests/proxmox/setup_pve_test.sh
  2. Run this:       pytest tests/proxmox/test_proxmox_e2e.py -v

Environment:
  PVE_SSH_PORT  — SSH port on localhost (default: 2250)
  PVE_PASSWORD  — root password (default: ozmatest123)
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

PVE_SSH_PORT = int(os.environ.get("PVE_SSH_PORT", "2250"))
PVE_PASSWORD = os.environ.get("PVE_PASSWORD", "ozmatest123")
VMID = 100


def ssh(cmd: str, timeout: int = 30) -> str:
    """Run a command inside the PVE VM via SSH."""
    result = subprocess.run(
        ["sshpass", "-p", PVE_PASSWORD, "ssh",
         "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR",
         "-p", str(PVE_SSH_PORT),
         "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0 and result.stderr:
        raise RuntimeError(f"SSH command failed: {result.stderr.strip()}")
    return result.stdout.strip()


def ssh_python(code: str, timeout: int = 30) -> str:
    """Run Python code inside the PVE VM via stdin pipe."""
    result = subprocess.run(
        ["sshpass", "-p", PVE_PASSWORD, "ssh",
         "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR",
         "-p", str(PVE_SSH_PORT),
         "root@localhost", "python3"],
        input=code, capture_output=True, text=True, timeout=timeout,
    )
    if result.returncode != 0 and result.stderr:
        raise RuntimeError(f"Python failed: {result.stderr.strip()}")
    return result.stdout.strip()


def pve_available() -> bool:
    """Check if the PVE VM is reachable."""
    try:
        ssh("pveversion", timeout=10)
        return True
    except Exception:
        return False


pytestmark = pytest.mark.skipif(
    not pve_available(),
    reason=f"PVE VM not reachable on SSH port {PVE_SSH_PORT}",
)


# ── PVE API ──────────────────────────────────────────────────────────────

class TestPVEAPI:
    """Verify PVE is running and accessible."""

    def test_pve_version(self):
        out = ssh("pveversion")
        assert "pve-manager" in out

    def test_pve_services(self):
        out = ssh("systemctl is-active pve-cluster pvedaemon pveproxy pvestatd")
        for line in out.splitlines():
            assert line.strip() == "active"

    def test_node_list(self):
        out = ssh("pvesh get /nodes --output-format json")
        nodes = json.loads(out)
        assert len(nodes) >= 1

    def test_vm_list(self):
        out = ssh("pvesh get /nodes/localhost/qemu --output-format json")
        vms = json.loads(out)
        assert any(v["vmid"] == VMID for v in vms)

    def test_vm_running(self):
        out = ssh(f"pvesh get /nodes/localhost/qemu/{VMID}/status/current --output-format json")
        status = json.loads(out)
        assert status["status"] == "running"
        assert status["name"] == "doom-test"


# ── VM Discovery ─────────────────────────────────────────────────────────

class TestDiscovery:
    """Test VM discovery via QMP sockets and PVE config."""

    def test_qmp_socket_exists(self):
        out = ssh(f"test -S /var/run/qemu-server/{VMID}.qmp && echo yes")
        assert out == "yes"

    def test_ozma_qmp_socket_exists(self):
        out = ssh(f"test -S /var/run/ozma/vm{VMID}-ctrl.qmp && echo yes")
        assert out == "yes"

    def test_vm_config_readable(self):
        out = ssh(f"cat /etc/pve/qemu-server/{VMID}.conf")
        assert "name: doom-test" in out
        assert "ostype: l26" in out

    def test_discovery_via_filesystem(self):
        code = """
import re
from pathlib import Path
qmp_dir = Path('/var/run/qemu-server')
found = []
for sock in qmp_dir.iterdir():
    if sock.suffix == '.qmp' and sock.is_socket():
        vmid = sock.stem
        conf = Path(f'/etc/pve/qemu-server/{vmid}.conf')
        if conf.exists():
            text = conf.read_text()
            m = re.search(r'^name:\\s*(.+)', text, re.MULTILINE)
            name = m.group(1).strip() if m else f'vm{vmid}'
            found.append(f'{vmid}:{name}')
print(','.join(found))
"""
        out = ssh_python(code)
        assert f"{VMID}:doom-test" in out


# ── QMP ──────────────────────────────────────────────────────────────────

class TestQMP:
    """Test QMP socket connectivity and commands."""

    def _qmp_cmd(self, socket_path: str, command: str, args: dict | None = None) -> str:
        code = f"""
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect({socket_path!r})
s.recv(4096)
s.sendall(json.dumps({{"execute": "qmp_capabilities"}}).encode() + b"\\n")
s.recv(4096)
cmd = {{"execute": {command!r}}}
if {args!r}:
    cmd["arguments"] = {args!r}
s.sendall(json.dumps(cmd).encode() + b"\\n")
import time; time.sleep(0.5)
print(s.recv(4096).decode())
s.close()
"""
        return ssh_python(code)

    def test_pve_qmp_status(self):
        out = self._qmp_cmd(f"/var/run/qemu-server/{VMID}.qmp", "query-status")
        data = json.loads(out)
        assert data["return"]["running"] is True

    def test_ozma_qmp_status(self):
        out = self._qmp_cmd(f"/var/run/ozma/vm{VMID}-ctrl.qmp", "query-status")
        data = json.loads(out)
        assert data["return"]["running"] is True

    def test_display_is_dbus_p2p(self):
        out = self._qmp_cmd(f"/var/run/ozma/vm{VMID}-ctrl.qmp", "query-display-options")
        data = json.loads(out)
        assert data["return"]["type"] == "dbus"
        assert data["return"]["p2p"] is True

    def test_screendump(self):
        code = f"""
import socket, json, os, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect('/var/run/qemu-server/{VMID}.qmp')
s.recv(4096)
s.sendall(json.dumps({{"execute": "qmp_capabilities"}}).encode() + b"\\n")
s.recv(4096)
tmp = '/tmp/e2e-snap.ppm'
s.sendall(json.dumps({{"execute": "screendump", "arguments": {{"filename": tmp}}}}).encode() + b"\\n")
time.sleep(1)
s.recv(4096)
size = os.path.getsize(tmp) if os.path.exists(tmp) else 0
os.unlink(tmp) if os.path.exists(tmp) else None
print(size)
s.close()
"""
        out = ssh_python(code)
        assert int(out) > 1000  # at least 1KB

    def test_keyboard_input(self):
        out = self._qmp_cmd(
            f"/var/run/qemu-server/{VMID}.qmp",
            "input-send-event",
            {"events": [{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "ret"}}}]},
        )
        assert '"return"' in out

    def test_mouse_input(self):
        out = self._qmp_cmd(
            f"/var/run/qemu-server/{VMID}.qmp",
            "input-send-event",
            {"events": [
                {"type": "abs", "data": {"axis": "x", "value": 16384}},
                {"type": "abs", "data": {"axis": "y", "value": 16384}},
            ]},
        )
        assert '"return"' in out


# ── D-Bus p2p Display ────────────────────────────────────────────────────

class TestDBusDisplay:
    """Test D-Bus p2p display connection via QMP add_client."""

    def test_add_client_protocol(self):
        """QMP add_client accepts @dbus-display protocol on QEMU 10+."""
        code = f"""
import socket, json, array
a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect('/var/run/qemu-server/{VMID}.qmp')
s.recv(4096)
s.sendall(json.dumps({{"execute": "qmp_capabilities"}}).encode() + b"\\n")
s.recv(4096)
fds = array.array('i', [b.fileno()])
msg = json.dumps({{"execute": "getfd", "arguments": {{"fdname": "test-fd"}}}}).encode() + b"\\n"
s.sendmsg([msg], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])
r1 = json.loads(s.recv(4096))
b.close()
s.sendall(json.dumps({{
    "execute": "add_client",
    "arguments": {{"protocol": "@dbus-display", "fdname": "test-fd", "skipauth": True}}
}}).encode() + b"\\n")
r2 = json.loads(s.recv(4096))
a.close(); s.close()
print(json.dumps({{"getfd": r1, "add_client": r2}}))
"""
        out = ssh_python(code)
        data = json.loads(out)
        assert "return" in data["getfd"]
        assert "return" in data["add_client"], f"add_client failed: {data['add_client']}"

    def test_dbus_auth_external(self):
        """D-Bus EXTERNAL auth succeeds on the p2p connection."""
        code = f"""
import socket, json, array, os, time
a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect('/var/run/qemu-server/{VMID}.qmp')
s.recv(4096)
s.sendall(json.dumps({{"execute": "qmp_capabilities"}}).encode() + b"\\n")
s.recv(4096)
fds = array.array('i', [b.fileno()])
msg = json.dumps({{"execute": "getfd", "arguments": {{"fdname": "auth-fd"}}}}).encode() + b"\\n"
s.sendmsg([msg], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])
s.recv(4096)
b.close()
s.sendall(json.dumps({{
    "execute": "add_client",
    "arguments": {{"protocol": "@dbus-display", "fdname": "auth-fd"}}
}}).encode() + b"\\n")
s.recv(4096)
a.settimeout(3)
time.sleep(0.1)
uid_hex = str(os.getuid()).encode().hex()
a.sendall(b"\\0AUTH EXTERNAL " + uid_hex.encode() + b"\\r\\n")
time.sleep(0.3)
resp = a.recv(4096)
a.close(); s.close()
print("OK" if b"OK" in resp else f"FAIL: {{resp}}")
"""
        out = ssh_python(code)
        assert out == "OK"


# ── Display Service ──────────────────────────────────────────────────────

class TestDisplayService:
    """Test the ozma display service initialization and API."""

    def test_service_init(self):
        code = f"""
import sys
sys.path.insert(0, '/usr/lib/ozma-proxmox')
from importlib.util import spec_from_file_location, module_from_spec
spec = spec_from_file_location('ds', '/usr/lib/ozma-proxmox/display-service.py')
mod = module_from_spec(spec)
spec.loader.exec_module(mod)
svc = mod.VMDisplayService(vmid={VMID})
print(f'{{svc.vmid}}:{{svc.api_port}}:{{svc.hid_port}}')
"""
        out = ssh_python(code)
        parts = out.split(":")
        assert parts[0] == str(VMID)
        assert parts[1] == str(7390 + VMID)
        assert parts[2] == str(7340 + VMID)

    def test_service_http_api(self):
        """Start display service temporarily and test HTTP endpoints."""
        code = f"""
import asyncio, sys, json
sys.path.insert(0, '/usr/lib/ozma-proxmox')
from importlib.util import spec_from_file_location, module_from_spec
spec = spec_from_file_location('ds', '/usr/lib/ozma-proxmox/display-service.py')
mod = module_from_spec(spec)
spec.loader.exec_module(mod)

async def test():
    svc = mod.VMDisplayService(vmid={VMID})
    await svc.start()

    import aiohttp
    async with aiohttp.ClientSession() as session:
        # Health
        async with session.get(f'http://localhost:{{svc.api_port}}/health') as r:
            health = await r.json()
            assert health['ok'] is True

        # Display info
        async with session.get(f'http://localhost:{{svc.api_port}}/display/info') as r:
            info = await r.json()
            assert info['vmid'] == {VMID}

        # Snapshot
        async with session.get(f'http://localhost:{{svc.api_port}}/display/snapshot') as r:
            if r.status == 200:
                data = await r.read()
                print(f'OK:{{len(data)}}')
            else:
                body = await r.json()
                print(f'NO_FRAME:{{r.status}}')

asyncio.run(test())
"""
        out = ssh_python(code, timeout=30)
        assert out.startswith("OK:") or out.startswith("NO_FRAME:")
        if out.startswith("OK:"):
            size = int(out.split(":")[1])
            assert size > 1000


# ── Perl Module ──────────────────────────────────────────────────────────

class TestPerlModule:
    """Test the Perl QEMU hook module."""

    def test_module_installed(self):
        out = ssh("test -f /usr/share/perl5/PVE/QemuServer/Ozma.pm && echo yes")
        assert out == "yes"

    def test_module_syntax(self):
        out = ssh("perl -c /usr/share/perl5/PVE/QemuServer/Ozma.pm 2>&1")
        assert "syntax OK" in out or "Can't locate PVE" in out

    def test_module_has_hooks(self):
        out = ssh("grep -c 'sub on_vm_' /usr/share/perl5/PVE/QemuServer/Ozma.pm")
        count = int(out)
        assert count >= 3  # on_vm_start, on_vm_stop, on_vm_migrate

    def test_module_generates_dbus_args(self):
        out = ssh("grep 'dbus,p2p=yes' /usr/share/perl5/PVE/QemuServer/Ozma.pm")
        assert "dbus,p2p=yes" in out

    def test_module_generates_qmp_socket(self):
        out = ssh("grep 'ozma-mon' /usr/share/perl5/PVE/QemuServer/Ozma.pm")
        assert "ozma-mon" in out


# ── VM Lifecycle ─────────────────────────────────────────────────────────

class TestLifecycle:
    """Test VM stop/start cycle preserves ozma integration."""

    def test_stop_start_cycle(self):
        # Stop
        ssh(f"qm stop {VMID}", timeout=30)
        time.sleep(3)

        # Verify stopped
        out = ssh(f"qm status {VMID}")
        assert "stopped" in out

        # Verify sockets gone
        out = ssh(f"test -S /var/run/qemu-server/{VMID}.qmp && echo yes || echo no")
        assert out == "no"

        # Start
        ssh(f"qm start {VMID}", timeout=30)
        time.sleep(5)

        # Verify running
        out = ssh(f"qm status {VMID}")
        assert "running" in out

        # Verify sockets recreated
        out = ssh(f"test -S /var/run/qemu-server/{VMID}.qmp && echo yes || echo no")
        assert out == "yes"
        out = ssh(f"test -S /var/run/ozma/vm{VMID}-ctrl.qmp && echo yes || echo no")
        assert out == "yes"


# ── QEMU Version Compatibility ───────────────────────────────────────────

class TestQEMUCompat:
    """Test QEMU version-specific features."""

    def test_qemu_version(self):
        out = ssh("qemu-system-x86_64 --version | head -1")
        assert "QEMU" in out
        # Extract version
        import re
        m = re.search(r"(\d+)\.(\d+)", out)
        assert m
        major = int(m.group(1))
        assert major >= 8  # need at least QEMU 8 for D-Bus display

    def test_dbus_display_available(self):
        out = ssh("qemu-system-x86_64 -display help 2>&1")
        assert "dbus" in out


if __name__ == "__main__":
    pytest.main([__file__, "-v", "--tb=short"])
