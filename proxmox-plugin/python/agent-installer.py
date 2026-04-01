#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
One-click agent installer for Proxmox VMs.

Installs the ozma agent inside a running VM without manual intervention.
Three installation methods, tried in order:

  1. QEMU Guest Agent (qga) — if qemu-ga is running inside the VM
     Uses guest-exec to run the installer. Fastest, most reliable.

  2. Virtio-serial — if virtio-serial device is configured
     Sends installer payload through the serial port.

  3. Virtual USB drive — mount a FAT32 image with the installer
     via QMP, then trigger autorun or use RPA to execute it.

The installer payload includes:
  - Python embeddable package (no system install needed)
  - ozma-agent wheel (pre-built)
  - Bootstrap script that unpacks and starts the agent

Usage:
  # From Proxmox UI (via API)
  POST /api2/json/ozma/agent/install?vmid=100

  # From command line
  python3 agent-installer.py --vmid 100 --controller https://ozma.local
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

log = logging.getLogger("ozma.proxmox.agent_installer")


class AgentInstaller:
    """Install the ozma agent inside a VM."""

    def __init__(self, vmid: int, controller_url: str = "",
                 qmp_socket: str = "") -> None:
        self.vmid = vmid
        self.controller_url = controller_url or "http://localhost:7380"
        self.qmp_socket = qmp_socket or f"/var/run/qemu-server/{vmid}.qmp"
        self._method = "unknown"

    async def install(self) -> dict:
        """Install the agent. Returns status dict."""
        # Try methods in order of preference
        result = await self._try_guest_agent()
        if result["ok"]:
            return result

        result = await self._try_usb_media()
        if result["ok"]:
            return result

        return {"ok": False, "error": "No installation method available",
                "hint": "Install qemu-guest-agent inside the VM, or ensure the VM is running"}

    async def _try_guest_agent(self) -> dict:
        """Install via QEMU Guest Agent (qga)."""
        self._method = "guest-agent"

        # Check if guest agent is responding
        try:
            result = subprocess.run(
                ["qm", "agent", str(self.vmid), "ping"],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode != 0:
                return {"ok": False, "error": "Guest agent not responding"}
        except Exception:
            return {"ok": False, "error": "Guest agent not available"}

        # Detect guest OS
        try:
            os_info = subprocess.check_output(
                ["qm", "agent", str(self.vmid), "get-osinfo"],
                text=True, timeout=10,
            )
            os_data = json.loads(os_info)
            os_id = os_data.get("result", {}).get("id", "")
            is_windows = "mswindows" in os_id or "windows" in os_id.lower()
        except Exception:
            is_windows = False  # assume Linux

        log.info("VM %d: guest agent responding, OS=%s", self.vmid,
                 "Windows" if is_windows else "Linux")

        if is_windows:
            return await self._install_windows_via_ga()
        else:
            return await self._install_linux_via_ga()

    async def _install_windows_via_ga(self) -> dict:
        """Install agent on Windows via guest-exec."""
        try:
            # Step 1: Write bootstrap script to the VM
            bootstrap = self._windows_bootstrap_script()
            self._ga_write_file(r"C:\ozma-bootstrap.ps1", bootstrap)

            # Step 2: Execute the bootstrap via PowerShell
            self._ga_exec(
                "powershell.exe",
                ["-ExecutionPolicy", "Bypass", "-File", r"C:\ozma-bootstrap.ps1"],
                timeout=300,
            )

            return {"ok": True, "method": "guest-agent", "os": "windows",
                    "message": "Agent installing via PowerShell"}

        except Exception as e:
            return {"ok": False, "error": str(e), "method": "guest-agent"}

    async def _install_linux_via_ga(self) -> dict:
        """Install agent on Linux via guest-exec."""
        try:
            # One-liner: pip install ozma-agent from PyPI (or controller URL)
            script = self._linux_bootstrap_script()
            self._ga_write_file("/tmp/ozma-install.sh", script)
            self._ga_exec("bash", ["/tmp/ozma-install.sh"], timeout=300)

            return {"ok": True, "method": "guest-agent", "os": "linux",
                    "message": "Agent installing via bash"}

        except Exception as e:
            return {"ok": False, "error": str(e), "method": "guest-agent"}

    async def _try_usb_media(self) -> dict:
        """Install via virtual USB drive with installer payload."""
        self._method = "usb-media"

        # Build a minimal FAT32 image with the agent installer
        img_path = f"/tmp/ozma-agent-install-{self.vmid}.img"
        self._build_installer_image(img_path)

        # Attach via QMP
        try:
            qmp_cmd = json.dumps({
                "execute": "blockdev-add",
                "arguments": {
                    "driver": "file",
                    "node-name": "ozma-installer-file",
                    "filename": img_path,
                    "read-only": True,
                }
            })
            subprocess.run(
                ["qm", "monitor", str(self.vmid), qmp_cmd],
                capture_output=True, timeout=10,
            )

            qmp_cmd2 = json.dumps({
                "execute": "device_add",
                "arguments": {
                    "driver": "usb-storage",
                    "id": "ozma-installer",
                    "drive": "ozma-installer-file",
                    "removable": True,
                }
            })
            subprocess.run(
                ["qm", "monitor", str(self.vmid), qmp_cmd2],
                capture_output=True, timeout=10,
            )

            return {"ok": True, "method": "usb-media",
                    "message": f"Installer USB attached. Run D:\\install.bat (Windows) or /media/usb/install.sh (Linux) inside the VM."}

        except Exception as e:
            return {"ok": False, "error": str(e), "method": "usb-media"}

    # ── Helper: guest agent commands ──────────────────────────────────

    def _ga_write_file(self, path: str, content: str) -> None:
        """Write a file inside the VM via guest agent."""
        import base64
        b64 = base64.b64encode(content.encode()).decode()
        subprocess.run(
            ["qm", "agent", str(self.vmid), "file-write",
             "--file", path, "--content", b64, "--encode", "base64"],
            check=True, timeout=30,
        )

    def _ga_exec(self, program: str, args: list[str], timeout: int = 60) -> str:
        """Execute a command inside the VM via guest agent."""
        cmd = json.dumps({"path": program, "arg": args, "capture-output": True})
        result = subprocess.run(
            ["qm", "agent", str(self.vmid), "exec", "--", program] + args,
            capture_output=True, text=True, timeout=timeout,
        )
        return result.stdout

    # ── Bootstrap scripts ─────────────────────────────────────────────

    def _windows_bootstrap_script(self) -> str:
        return f"""# Ozma Agent Bootstrap (Windows)
$ErrorActionPreference = "Continue"
Write-Host "=== Ozma Agent Install ==="

# Download Python embeddable if not installed
$py = Get-Command python -ErrorAction SilentlyContinue
if (-not $py) {{
    Write-Host "Downloading Python..."
    Invoke-WebRequest -Uri "https://www.python.org/ftp/python/3.13.5/python-3.13.5-embed-amd64.zip" -OutFile "$env:TEMP\\python.zip"
    Expand-Archive -Path "$env:TEMP\\python.zip" -DestinationPath "C:\\ozma-python" -Force
    $env:PATH = "C:\\ozma-python;$env:PATH"
}}

# Install pip
python -m ensurepip 2>$null
python -m pip install --upgrade pip 2>$null

# Install ozma-agent
python -m pip install ozma-agent

# Run agent install
ozma-agent install --controller "{self.controller_url}" --name "$env:COMPUTERNAME"

Write-Host "=== Done ==="
"""

    def _linux_bootstrap_script(self) -> str:
        return f"""#!/bin/bash
set -e
echo "=== Ozma Agent Install ==="

# Install pip if needed
command -v pip3 >/dev/null 2>&1 || {{
    apt-get update -qq && apt-get install -y -qq python3-pip 2>/dev/null || \
    dnf install -y python3-pip 2>/dev/null || \
    pacman -Sy --noconfirm python-pip 2>/dev/null
}}

# Install ozma-agent
pip3 install ozma-agent

# Install as systemd service
ozma-agent install --controller "{self.controller_url}" --name "$(hostname)"

echo "=== Done ==="
"""

    def _build_installer_image(self, output_path: str) -> None:
        """Build a small FAT32 image with the agent installer."""
        size_mb = 50
        subprocess.run(["dd", "if=/dev/zero", f"of={output_path}",
                        "bs=1M", f"count={size_mb}"],
                       capture_output=True, check=True)
        subprocess.run(["mkfs.vfat", "-F", "32", "-n", "OZMA-AGENT", output_path],
                       capture_output=True, check=True)

        mount_point = tempfile.mkdtemp()
        try:
            subprocess.run(["sudo", "mount", "-o", "loop", output_path, mount_point],
                           check=True, capture_output=True)

            # Write install scripts
            Path(mount_point, "install.bat").write_text(
                f'@echo off\npowershell -ExecutionPolicy Bypass -Command '
                f'"Invoke-WebRequest -Uri {self.controller_url}/api/v1/agent/bootstrap/windows '
                f'-OutFile %TEMP%\\ozma-install.ps1; & %TEMP%\\ozma-install.ps1"\n'
            )
            Path(mount_point, "install.sh").write_text(
                f'#!/bin/bash\ncurl -fsSL {self.controller_url}/api/v1/agent/bootstrap/linux | bash\n'
            )
            os.chmod(str(Path(mount_point, "install.sh")), 0o755)

            # Write controller URL
            Path(mount_point, "controller.txt").write_text(self.controller_url)

        finally:
            subprocess.run(["sudo", "umount", mount_point], capture_output=True)
            os.rmdir(mount_point)


async def main():
    import argparse
    p = argparse.ArgumentParser(description="Install ozma agent in a VM")
    p.add_argument("--vmid", required=True, type=int)
    p.add_argument("--controller", default="http://localhost:7380")
    p.add_argument("--qmp", default="")
    args = p.parse_args()

    logging.basicConfig(level=logging.INFO)

    installer = AgentInstaller(args.vmid, args.controller, args.qmp)
    result = await installer.install()
    print(json.dumps(result, indent=2))
    sys.exit(0 if result["ok"] else 1)


if __name__ == "__main__":
    asyncio.run(main())
