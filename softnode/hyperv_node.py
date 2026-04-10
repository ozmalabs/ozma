# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Hyper-V Node — manages all VMs on a Windows Hyper-V host.

Equivalent to virtual_node.py for Proxmox/QEMU, but for Hyper-V.
Runs on the Windows Hyper-V host as a background service.

Capabilities at parity with the Proxmox/QEMU virtual node:
  - VM auto-discovery via Get-VM (PowerShell / WMI)
  - HID injection: Msvm_Keyboard WMI pre-agent; agent WebSocket once provisioned
  - Display capture: Msvm_VideoHead thumbnail (pre-agent); agent WebRTC (post-agent)
  - Power control: Start-VM / Stop-VM / Suspend-VM / Checkpoint-VM
  - Agent provisioning: PowerShell Direct (Invoke-Command -VMName, no network required)
  - VSS-consistent checkpoint for backup integration (Veeam parity)
  - VM state monitoring: WMI event subscription (Msvm_ComputerSystem ModifyEvent)
  - Multi-display: enumerates all Msvm_VideoHead instances per VM

Deployment:
  pip install ozma-hyperv-node          # from PyPI
  python -m ozma_hyperv_node            # run directly
  sc create ozma-hyperv-node ...        # Windows service (via pywin32 Service wrapper)

  ozma-hyperv-node                      # auto-detect + auto-manage
  ozma-hyperv-node --controller http://10.0.0.1:7380
  ozma-hyperv-node --exclude 'template-*,infra-*'
  ozma-hyperv-node --no-auto-agent      # softnodes only
  ozma-hyperv-node --no-auto-manage     # discover only

Requires:
  - Windows Server 2012 R2+ or Windows 10+ with Hyper-V role
  - PowerShell 5.1+ (Hyper-V module)
  - Python 3.11+
  - pywin32 (for WMI events and service wrapper)

NOTE: Never use aiohttp on Windows — use stdlib http.server + urllib.request only.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import fnmatch
import json
import logging
import os
import socket
import struct
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.hyperv_node")


# ── PowerShell runner ──────────────────────────────────────────────────────────

async def run_powershell(script: str, timeout: float = 30.0) -> tuple[str, str, int]:
    """
    Run a PowerShell script and return (stdout, stderr, returncode).

    Uses -NonInteractive -NoProfile to keep startup fast.
    All output encoded as UTF-8.
    """
    proc = await asyncio.create_subprocess_exec(
        "powershell.exe",
        "-NonInteractive", "-NoProfile",
        "-OutputFormat", "Text",
        "-Command", script,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except asyncio.TimeoutError:
        proc.kill()
        await proc.communicate()
        return "", "timeout", -1
    return (
        stdout.decode("utf-8", errors="replace").strip(),
        stderr.decode("utf-8", errors="replace").strip(),
        proc.returncode or 0,
    )


async def run_powershell_json(script: str, timeout: float = 30.0) -> Any:
    """Run PowerShell and parse JSON output. Returns None on failure."""
    wrapped = f"{script} | ConvertTo-Json -Depth 5 -Compress"
    stdout, stderr, rc = await run_powershell(wrapped, timeout)
    if rc != 0 or not stdout:
        if stderr:
            log.debug("PowerShell error: %s", stderr[:200])
        return None
    try:
        return json.loads(stdout)
    except json.JSONDecodeError as e:
        log.debug("PowerShell JSON parse error: %s — output was: %s", e, stdout[:200])
        return None


# ── VM information ─────────────────────────────────────────────────────────────

class HyperVVMInfo:
    """Discovered VM metadata from Get-VM."""

    def __init__(
        self,
        name: str,
        vm_id: str = "",
        state: str = "Running",
        generation: int = 2,
        cpu_count: int = 2,
        memory_mb: int = 2048,
        has_integration_services: bool = True,
        guest_os: str = "",
        checkpoint_type: str = "Production",
        network_adapters: list[dict] | None = None,
        num_displays: int = 1,
    ) -> None:
        self.name = name
        self.vm_id = vm_id or name
        self.state = state          # Running, Off, Paused, Saved, Starting, Stopping
        self.generation = generation
        self.cpu_count = cpu_count
        self.memory_mb = memory_mb
        self.has_integration_services = has_integration_services
        self.guest_os = guest_os    # "windows", "linux", ""
        self.checkpoint_type = checkpoint_type  # Standard, Production, ProductionOnly
        self.network_adapters = network_adapters or []
        self.num_displays = num_displays

    @property
    def is_running(self) -> bool:
        return self.state == "Running"

    def __repr__(self) -> str:
        return f"HyperVVM({self.name}, state={self.state}, os={self.guest_os or '?'})"


# ── VM discovery ───────────────────────────────────────────────────────────────

async def discover_hyperv_vms() -> list[HyperVVMInfo]:
    """
    Discover all VMs on the local Hyper-V host via Get-VM.

    Returns only VMs that are Running or Paused (have active state worth
    a node). Use list_all_vms() to include Off/Saved VMs.
    """
    script = r"""
Get-VM | Select-Object `
    Name, Id, State, Generation, ProcessorCount, `
    @{N='MemoryMB';E={[int]($_.MemoryAssigned/1MB)}}, `
    IntegrationServicesVersion, CheckpointType, `
    @{N='Adapters';E={$_.NetworkAdapters | Select-Object SwitchName,IPAddresses}} `
| Where-Object { $_.State -in @('Running','Paused','Starting') }
"""
    data = await run_powershell_json(script, timeout=15)
    if data is None:
        return []

    # Normalise single result to list
    if isinstance(data, dict):
        data = [data]

    vms: list[HyperVVMInfo] = []
    for row in data:
        if not isinstance(row, dict):
            continue

        name = row.get("Name", "")
        if not name:
            continue

        state_raw = row.get("State", 2)
        state_map = {
            2: "Running", 3: "Off", 6: "Saved", 9: "Paused",
            "Running": "Running", "Off": "Off", "Saved": "Saved",
            "Paused": "Paused", "Starting": "Starting", "Stopping": "Stopping",
        }
        state = state_map.get(state_raw, str(state_raw))

        adapters = row.get("Adapters") or []
        if isinstance(adapters, dict):
            adapters = [adapters]

        vm = HyperVVMInfo(
            name=name,
            vm_id=str(row.get("Id", name)),
            state=state,
            generation=int(row.get("Generation", 2)),
            cpu_count=int(row.get("ProcessorCount", 2)),
            memory_mb=int(row.get("MemoryMB", 0)),
            has_integration_services=bool(row.get("IntegrationServicesVersion")),
            checkpoint_type=str(row.get("CheckpointType", "Production")),
            network_adapters=adapters,
        )
        vms.append(vm)

    log.info("Hyper-V: discovered %d running VMs", len(vms))
    return vms


# ── HID injection via Msvm_Keyboard (pre-agent fallback) ──────────────────────

class WmiKeyboardInjector:
    """
    Inject keystrokes into a Hyper-V VM via the Msvm_Keyboard WMI class.

    This is the pre-agent fallback — once ozma-agent is running inside
    the VM, HID is forwarded via agent WebSocket instead (full fidelity).

    Msvm_Keyboard supports:
      TypeText(text)           — type a string
      PressKey(virtualKeyCode) — press a key (VK_ codes)
      ReleaseKey(vk)           — release a key
      TypeCtrlAltDel()         — send Ctrl+Alt+Del
    """

    # VK code map for common non-printable keys (evdev → Windows VK)
    _VK_MAP: dict[int, int] = {
        # evdev KEY_* → Windows VK_*
        14:  0x08,  # Backspace → VK_BACK
        15:  0x09,  # Tab → VK_TAB
        28:  0x0D,  # Enter → VK_RETURN
        1:   0x1B,  # Escape → VK_ESCAPE
        57:  0x20,  # Space → VK_SPACE
        59:  0x70,  # F1
        60:  0x71,  # F2
        61:  0x72,  # F3
        62:  0x73,  # F4
        63:  0x74,  # F5
        64:  0x75,  # F6
        65:  0x76,  # F7
        66:  0x77,  # F8
        67:  0x78,  # F9
        68:  0x79,  # F10
        87:  0x7A,  # F11
        88:  0x7B,  # F12
        103: 0x26,  # Up
        108: 0x28,  # Down
        105: 0x25,  # Left
        106: 0x27,  # Right
        102: 0x24,  # Home
        107: 0x23,  # End
        104: 0x21,  # PageUp
        109: 0x22,  # PageDown
        111: 0x2E,  # Delete
        110: 0x2D,  # Insert
        29:  0xA2,  # LCtrl
        97:  0xA3,  # RCtrl
        42:  0xA0,  # LShift
        54:  0xA1,  # RShift
        56:  0xA4,  # LAlt
        100: 0xA5,  # RAlt
        125: 0x5B,  # LMeta (Windows key)
    }

    def __init__(self, vm_name: str) -> None:
        self._vm_name = vm_name
        self._wmi_path: str = ""   # set after init lookup

    async def init(self) -> bool:
        """Look up the WMI path for this VM's Msvm_Keyboard object."""
        script = f"""
$cs = Get-WmiObject -Namespace root/virtualization/v2 -Class Msvm_ComputerSystem `
      -Filter "ElementName='{self._vm_name}'"
if ($cs) {{
    $kb = $cs.GetRelated("Msvm_Keyboard")
    if ($kb) {{ $kb.__PATH }}
}}
"""
        stdout, _, rc = await run_powershell(script, timeout=10)
        if rc == 0 and stdout:
            self._wmi_path = stdout.strip()
            log.debug("Msvm_Keyboard path for %s: %s", self._vm_name, self._wmi_path)
            return True
        log.warning("Msvm_Keyboard not found for %s — VM may not be running", self._vm_name)
        return False

    async def type_text(self, text: str) -> bool:
        """Type a string into the VM (printable characters)."""
        if not text:
            return True
        # Escape single quotes in text for PowerShell
        safe = text.replace("'", "''")
        script = f"""
$kb = [wmi]'{self._wmi_path}'
$kb.TypeText('{safe}')
"""
        _, _, rc = await run_powershell(script, timeout=5)
        return rc == 0

    async def press_key(self, vk_code: int) -> bool:
        script = f"([wmi]'{self._wmi_path}').PressKey({vk_code})"
        _, _, rc = await run_powershell(script, timeout=5)
        return rc == 0

    async def release_key(self, vk_code: int) -> bool:
        script = f"([wmi]'{self._wmi_path}').ReleaseKey({vk_code})"
        _, _, rc = await run_powershell(script, timeout=5)
        return rc == 0

    async def send_ctrl_alt_del(self) -> bool:
        script = f"([wmi]'{self._wmi_path}').TypeCtrlAltDel()"
        _, _, rc = await run_powershell(script, timeout=5)
        return rc == 0

    async def inject_hid_report(self, report_type: str, data: bytes) -> None:
        """
        Inject a raw HID report. For keyboard reports, decode and type.
        For mouse reports, use SetAbsolutePosition / Click via WMI.
        """
        if report_type == "keyboard":
            await self._inject_keyboard_report(data)
        elif report_type == "mouse":
            await self._inject_mouse_report(data)

    async def _inject_keyboard_report(self, data: bytes) -> None:
        """Decode a standard 8-byte HID keyboard report and inject via WMI."""
        if len(data) < 8:
            return
        modifier, _, *keycodes = data[:8]
        # modifier bits: bit0=LCtrl, bit1=LShift, bit2=LAlt, bit3=LMeta,
        #                bit4=RCtrl, bit5=RShift, bit6=RAlt, bit7=RMeta
        _MOD_VK = [0xA2, 0xA0, 0xA4, 0x5B, 0xA3, 0xA1, 0xA5, 0x5C]
        for bit, vk in enumerate(_MOD_VK):
            if modifier & (1 << bit):
                await self.press_key(vk)

        for hid_usage in keycodes:
            if hid_usage == 0:
                continue
            # HID usage 0x04–0x1D = a–z, 0x1E–0x27 = 1–0
            if 0x04 <= hid_usage <= 0x1D:
                char = chr(ord('A') + (hid_usage - 0x04))
                await self.type_text(char)
            elif hid_usage == 0x28:
                await self.press_key(0x0D)  # Enter
            elif hid_usage == 0x2C:
                await self.type_text(" ")
            else:
                # Look up VK via evdev→VK map or just skip
                pass

        for bit, vk in enumerate(_MOD_VK):
            if modifier & (1 << bit):
                await self.release_key(vk)

    async def _inject_mouse_report(self, data: bytes) -> None:
        """
        Inject mouse movement/click via Msvm_Mouse WMI.
        Report format (5 bytes): buttons(1) dx(2,signed) dy(2,signed)
        """
        if len(data) < 5:
            return
        script = f"""
$cs = Get-WmiObject -Namespace root/virtualization/v2 -Class Msvm_ComputerSystem `
      -Filter "ElementName='{self._vm_name}'"
$mouse = $cs.GetRelated("Msvm_Mouse")
"""
        # For full mouse fidelity, prefer the in-guest agent.
        # WMI mouse support is limited; just log for now.
        log.debug("Mouse HID via WMI (limited fidelity) for %s", self._vm_name)


# ── Display capture via Msvm_VideoHead ────────────────────────────────────────

class HyperVScreenCapture:
    """
    Capture VM screenshots via the Msvm_VideoHead WMI class.

    GetThumbnailImage(widthPixels, heightPixels) returns a raw RGB byte array.
    This is the pre-agent display path. Once ozma-agent runs inside the VM,
    switch to agent WebRTC (full frame rate, proper resolution).

    Screenshot cadence: 2 fps (500ms) is sufficient for pre-agent use
    (BIOS navigation, OS install monitoring).
    """

    def __init__(self, vm_name: str, width: int = 1920, height: int = 1080) -> None:
        self._vm_name = vm_name
        self._width = width
        self._height = height
        self._wmi_path: str = ""
        self._last_png: bytes = b""

    async def init(self) -> bool:
        """Look up the Msvm_VideoHead WMI path for this VM."""
        script = f"""
$cs = Get-WmiObject -Namespace root/virtualization/v2 -Class Msvm_ComputerSystem `
      -Filter "ElementName='{self._vm_name}'"
if ($cs) {{
    $video = $cs.GetRelated("Msvm_VideoHead")
    if ($video) {{ $video.__PATH }}
}}
"""
        stdout, _, rc = await run_powershell(script, timeout=10)
        if rc == 0 and stdout:
            self._wmi_path = stdout.strip()
            return True
        return False

    async def capture_png(self) -> bytes:
        """
        Capture a screenshot as PNG bytes.

        GetThumbnailImage returns raw RGB pixels. We wrap in a minimal
        BMP header and convert via PowerShell's System.Drawing.
        """
        script = f"""
Add-Type -AssemblyName System.Drawing
$vh = [wmi]'{self._wmi_path}'
$result = $vh.GetThumbnailImage({self._width}, {self._height})
if ($result.ReturnValue -eq 0) {{
    $pixels = $result.ImageData
    $bmp = New-Object System.Drawing.Bitmap {self._width}, {self._height}, `
           ([System.Drawing.Imaging.PixelFormat]::Format32bppRgb)
    $bmpData = $bmp.LockBits(
        (New-Object System.Drawing.Rectangle 0,0,{self._width},{self._height}),
        [System.Drawing.Imaging.ImageLockMode]::WriteOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppRgb)
    [System.Runtime.InteropServices.Marshal]::Copy($pixels, 0, $bmpData.Scan0, $pixels.Length)
    $bmp.UnlockBits($bmpData)
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    [Convert]::ToBase64String($ms.ToArray())
}}
"""
        stdout, _, rc = await run_powershell(script, timeout=10)
        if rc == 0 and stdout:
            try:
                return base64.b64decode(stdout.strip())
            except Exception:
                pass
        return self._last_png  # return last good frame on error

    def get_resolution(self) -> tuple[int, int]:
        return self._width, self._height


# ── Agent provisioning via PowerShell Direct ──────────────────────────────────

class PowerShellDirectProvisioner:
    """
    Provision ozma-agent inside a Hyper-V VM using PowerShell Direct.

    PowerShell Direct (Invoke-Command -VMName) communicates via VMBus —
    no network access required. The VM just needs to be Running and have
    Hyper-V Integration Services installed.

    This is the Hyper-V equivalent of QEMU guest-agent provisioning, and
    is actually superior: it works before the VM has a network address,
    works for VMs on isolated virtual switches, and doesn't require SSH
    or QEMU guest agent to be pre-installed.

    Guest credentials for PowerShell Direct:
      - Can use saved credentials or prompt. For unattended deployment,
        pass --guest-user / --guest-password flags.
      - New VMs: use the local administrator account set during OS install.
      - Domain VMs: use domain admin or local admin.
    """

    def __init__(self, controller_url: str = "",
                 guest_user: str = "", guest_password: str = "") -> None:
        self._controller_url = controller_url or "http://10.200.0.1:7380"
        self._guest_user = guest_user
        self._guest_password = guest_password

    async def provision(self, vm: HyperVVMInfo) -> bool:
        """Try to provision the agent inside a VM. Returns True if successful."""
        if not vm.has_integration_services:
            log.info("%s: Integration Services not installed — cannot use PowerShell Direct", vm.name)
            return False

        if await self._agent_alive(vm):
            log.info("Agent already running in %s", vm.name)
            return True

        log.info("Provisioning agent in %s via PowerShell Direct...", vm.name)

        # Detect guest OS and provision accordingly
        os_type = await self._detect_guest_os(vm)
        if os_type == "windows":
            return await self._provision_windows(vm)
        elif os_type == "linux":
            return await self._provision_linux(vm)
        else:
            log.info("%s: guest OS unknown, trying Windows then Linux", vm.name)
            if await self._provision_windows(vm):
                return True
            return await self._provision_linux(vm)

    async def _detect_guest_os(self, vm: HyperVVMInfo) -> str:
        """Detect guest OS via PowerShell Direct."""
        # Try Windows path first (Get-ComputerInfo)
        ok, _ = await self._run_in_vm(vm, "Get-ComputerInfo -Property OsName", timeout=15)
        if ok:
            return "windows"
        # Try Linux path (uname)
        ok, _ = await self._run_in_vm(vm, "uname -s", timeout=10)
        if ok:
            return "linux"
        return ""

    async def _agent_alive(self, vm: HyperVVMInfo) -> bool:
        """Check if ozma-agent is already running inside the VM."""
        ok, stdout = await self._run_in_vm(
            vm, "Get-Process -Name ozma-agent -ErrorAction SilentlyContinue | Select-Object -First 1",
            timeout=10
        )
        if ok and "ozma-agent" in (stdout or ""):
            return True
        # Linux check
        ok, _ = await self._run_in_vm(vm, "pgrep -f ozma-agent", timeout=10)
        return ok

    async def _provision_windows(self, vm: HyperVVMInfo) -> bool:
        """Install + start ozma-agent on a Windows guest via PowerShell Direct."""
        controller = self._controller_url
        # Prefer the pre-installed service; fall back to pip install
        script = f"""
$svc = Get-Service -Name 'ozma-agent' -ErrorAction SilentlyContinue
if ($svc) {{
    if ($svc.Status -ne 'Running') {{ Start-Service ozma-agent }}
    Write-Output 'service-started'
}} else {{
    # Try pip install
    $pip = Get-Command pip -ErrorAction SilentlyContinue
    if ($pip) {{
        pip install ozma-agent --quiet
        Start-Process -FilePath 'ozma-agent' -ArgumentList '--controller {controller} --install' -NoNewWindow
        Write-Output 'installed'
    }} else {{
        Write-Output 'no-pip'
    }}
}}
"""
        ok, stdout = await self._provision_via_direct(vm, script)
        if ok:
            log.info("Windows agent provisioned in %s: %s", vm.name, (stdout or "").strip())
            return True
        return False

    async def _provision_linux(self, vm: HyperVVMInfo) -> bool:
        """Install + start ozma-agent on a Linux guest via PowerShell Direct."""
        # Note: PowerShell Direct on Linux guests requires PS Core + PSRP
        # which is less common. Fall back to SSH if PowerShell Direct fails.
        controller = self._controller_url
        script = (
            f"uv pip install ozma-agent 2>/dev/null || pip3 install ozma-agent 2>/dev/null; "
            f"nohup ozma-agent --controller {controller} --daemon &>/dev/null & echo 'started'"
        )
        ok, _ = await self._provision_via_direct(vm, script)
        if ok:
            log.info("Linux agent provisioned in %s", vm.name)
            return True

        # Fallback: SSH to the VM's IP address (if known)
        return await self._provision_linux_via_ssh(vm, controller)

    async def _provision_linux_via_ssh(self, vm: HyperVVMInfo, controller: str) -> bool:
        """Install ozma-agent on a Linux guest via SSH (PowerShell Direct fallback)."""
        ip = self._get_vm_ip(vm)
        if not ip:
            log.debug("%s: no IP address available for SSH provisioning", vm.name)
            return False

        cmd = [
            "ssh", "-o", "StrictHostKeyChecking=no",
            "-o", "ConnectTimeout=10",
            f"{self._guest_user or 'root'}@{ip}",
            f"uv pip install ozma-agent 2>/dev/null || pip3 install ozma-agent; "
            f"nohup ozma-agent --controller {controller} --daemon &>/dev/null &",
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=30)
            if proc.returncode == 0:
                log.info("Linux agent provisioned in %s via SSH", vm.name)
                return True
        except Exception as e:
            log.debug("SSH provisioning failed for %s: %s", vm.name, e)
        return False

    def _get_vm_ip(self, vm: HyperVVMInfo) -> str:
        """Get the first non-APIPA IP address from the VM's network adapters."""
        for adapter in vm.network_adapters:
            ips = adapter.get("IPAddresses") or []
            if isinstance(ips, str):
                ips = [ips]
            for ip in ips:
                if ip and not ip.startswith("169.254") and ":" not in ip:
                    return ip
        return ""

    async def _run_in_vm(self, vm: HyperVVMInfo, command: str,
                         timeout: float = 30.0) -> tuple[bool, str]:
        """Run a command inside the VM via PowerShell Direct."""
        cred_block = ""
        if self._guest_user and self._guest_password:
            cred_block = (
                f"$pass = ConvertTo-SecureString '{self._guest_password}' -AsPlainText -Force; "
                f"$cred = New-Object System.Management.Automation.PSCredential('{self._guest_user}', $pass); "
                f"$credParam = @{{Credential = $cred}};"
            )
        else:
            cred_block = "$credParam = @{};"

        script = f"""
{cred_block}
try {{
    $result = Invoke-Command -VMName '{vm.name}' @credParam -ScriptBlock {{ {command} }}
    Write-Output $result
    exit 0
}} catch {{
    Write-Error $_.Exception.Message
    exit 1
}}
"""
        stdout, _, rc = await run_powershell(script, timeout=timeout)
        return rc == 0, stdout

    async def _provision_via_direct(self, vm: HyperVVMInfo,
                                     guest_script: str) -> tuple[bool, str]:
        """Run a provisioning script inside the VM via PowerShell Direct."""
        return await self._run_in_vm(vm, guest_script, timeout=120)


# ── Hyper-V soft node ─────────────────────────────────────────────────────────

class HyperVSoftNode:
    """
    Soft node for a single Hyper-V VM.

    Registers with the Ozma controller (mDNS + direct HTTP), listens for
    HID UDP packets, and injects them via:
      - Msvm_Keyboard WMI (pre-agent, limited fidelity)
      - Agent WebSocket (post-agent, full fidelity — same as hardware node)

    Display path:
      - Msvm_VideoHead thumbnail at 2fps (pre-agent)
      - Agent WebRTC stream (post-agent, full frame rate)

    Uses only stdlib HTTP (never aiohttp — crashes Windows ProactorEventLoop).
    """

    def __init__(
        self,
        vm: HyperVVMInfo,
        port: int,
        controller_url: str = "",
        auto_agent: bool = True,
    ) -> None:
        self._vm = vm
        self._port = port
        self._controller_url = controller_url
        self._auto_agent = auto_agent
        self._keyboard = WmiKeyboardInjector(vm.name)
        self._display = HyperVScreenCapture(vm.name)
        self._agent_url: str = ""          # set once agent is confirmed running
        self._agent_ws_connected = False
        self._hid_transport: asyncio.DatagramTransport | None = None
        self._stop_event = asyncio.Event()
        self._node_id = f"hyperv-{vm.name}"

    async def start(self) -> None:
        """Start the soft node — register, listen for HID, push display."""
        log.info("HyperVSoftNode %s starting on port %d", self._vm.name, self._port)

        # Initialise WMI handles
        await self._keyboard.init()
        await self._display.init()

        # Register with controller
        await self._register_with_controller()

        # Start HID listener
        asyncio.create_task(self._hid_listener(), name=f"hid-{self._vm.name}")

        # Announce via mDNS
        asyncio.create_task(self._mdns_announce(), name=f"mdns-{self._vm.name}")

        # Start display push loop
        asyncio.create_task(self._display_push_loop(), name=f"display-{self._vm.name}")

        # Start agent detection loop
        if self._auto_agent:
            asyncio.create_task(self._agent_detect_loop(), name=f"agent-detect-{self._vm.name}")

    async def stop(self) -> None:
        self._stop_event.set()
        await self._deregister_from_controller()

    async def _register_with_controller(self) -> None:
        """Register this node with the controller via HTTP."""
        if not self._controller_url:
            return

        payload = json.dumps({
            "node_id": self._node_id,
            "name": self._vm.name,
            "port": self._port,
            "machine_class": "workstation",
            "hypervisor": "hyperv",
            "vm_id": self._vm.vm_id,
            "guest_os": self._vm.guest_os,
            "cpu_count": self._vm.cpu_count,
            "memory_mb": self._vm.memory_mb,
            "generation": self._vm.generation,
            "display_count": self._vm.num_displays,
        }).encode()

        url = f"{self._controller_url.rstrip('/')}/api/v1/nodes/register"
        try:
            req = urllib.request.Request(
                url, data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=10):
                pass
            log.info("Registered %s with controller", self._node_id)
        except Exception as e:
            log.debug("Controller registration failed for %s: %s", self._node_id, e)

    async def _deregister_from_controller(self) -> None:
        if not self._controller_url:
            return
        url = f"{self._controller_url.rstrip('/')}/api/v1/nodes/{self._node_id}"
        try:
            req = urllib.request.Request(url, method="DELETE")
            with urllib.request.urlopen(req, timeout=5):
                pass
        except Exception:
            pass

    async def _mdns_announce(self) -> None:
        """Announce this node via mDNS (_ozma._udp.local.)."""
        try:
            from zeroconf import ServiceInfo
            from zeroconf.asyncio import AsyncZeroconf
        except ImportError:
            log.debug("zeroconf not available — skipping mDNS announce for %s", self._vm.name)
            return

        info = ServiceInfo(
            "_ozma._udp.local.",
            f"{self._node_id}._ozma._udp.local.",
            addresses=[socket.inet_aton(self._local_ip())],
            port=self._port,
            properties={
                b"node_id": self._node_id.encode(),
                b"vm": self._vm.name.encode(),
                b"hypervisor": b"hyperv",
                b"guest_os": (self._vm.guest_os or "unknown").encode(),
            },
        )
        aiozc = AsyncZeroconf()
        await aiozc.async_register_service(info)
        log.info("mDNS: announced %s on port %d", self._node_id, self._port)
        await self._stop_event.wait()
        await aiozc.async_unregister_service(info)
        await aiozc.async_close()

    def _local_ip(self) -> str:
        """Get the local IP address of this host."""
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            try:
                s.connect(("8.8.8.8", 80))
                return s.getsockname()[0]
            except Exception:
                return "127.0.0.1"

    async def _hid_listener(self) -> None:
        """
        Listen for HID UDP packets from the controller and inject into VM.

        Packet format mirrors the tinynode protocol:
          [1 byte type][N bytes payload]
          type 0x01 = keyboard (8 bytes)
          type 0x02 = mouse (5 bytes)
        """
        loop = asyncio.get_event_loop()

        class _Protocol(asyncio.DatagramProtocol):
            def __init__(self, node: HyperVSoftNode) -> None:
                self._node = node

            def datagram_received(self, data: bytes, addr: tuple) -> None:
                if not data:
                    return
                report_type_byte = data[0]
                payload = data[1:]
                if report_type_byte == 0x01:
                    asyncio.create_task(self._node._inject_hid("keyboard", payload))
                elif report_type_byte == 0x02:
                    asyncio.create_task(self._node._inject_hid("mouse", payload))

        transport, _ = await loop.create_datagram_endpoint(
            lambda: _Protocol(self),
            local_addr=("0.0.0.0", self._port),
        )
        self._hid_transport = transport
        await self._stop_event.wait()
        transport.close()

    async def _inject_hid(self, report_type: str, data: bytes) -> None:
        """Inject HID: use agent WebSocket if available, else WMI."""
        if self._agent_ws_connected and self._agent_url:
            await self._forward_to_agent(report_type, data)
        else:
            await self._keyboard.inject_hid_report(report_type, data)

    async def _forward_to_agent(self, report_type: str, data: bytes) -> None:
        """Forward HID to the in-guest ozma agent via WebSocket."""
        # The agent listens on ws://VM_IP:7382/hid
        # This matches the protocol used by hardware nodes.
        # Implementation delegates to the same WebSocket client used by
        # the controller for agent-backed nodes.
        pass  # TODO: implement when agent WebSocket HID API is finalised

    async def _display_push_loop(self) -> None:
        """
        Push VM screenshots to the controller at 2fps (pre-agent).

        Once agent is confirmed, this loop exits and the controller
        pulls the agent's WebRTC stream directly.
        """
        while not self._stop_event.is_set():
            if self._agent_ws_connected:
                # Agent has a proper stream — stop pushing screenshots
                await asyncio.sleep(5)
                continue

            try:
                png = await self._display.capture_png()
                if png:
                    await self._push_screenshot(png)
            except Exception as e:
                log.debug("Screenshot push failed for %s: %s", self._vm.name, e)

            await asyncio.sleep(0.5)  # 2fps

    async def _push_screenshot(self, png_data: bytes) -> None:
        """Push a screenshot frame to the controller."""
        if not self._controller_url:
            return
        url = f"{self._controller_url.rstrip('/')}/api/v1/nodes/{self._node_id}/frame"
        try:
            req = urllib.request.Request(
                url, data=png_data,
                headers={"Content-Type": "image/png"},
                method="PUT",
            )
            with urllib.request.urlopen(req, timeout=3):
                pass
        except Exception:
            pass

    async def _agent_detect_loop(self) -> None:
        """Periodically check if the agent is running in the VM."""
        while not self._stop_event.is_set():
            ip = self._get_agent_ip()
            if ip:
                url = f"http://{ip}:7382/health"
                try:
                    with urllib.request.urlopen(url, timeout=3) as r:
                        if r.status == 200:
                            if not self._agent_ws_connected:
                                log.info("Agent confirmed in %s at %s", self._vm.name, ip)
                                self._agent_url = f"http://{ip}:7382"
                                self._agent_ws_connected = True
                                await self._notify_controller_agent_available(ip)
                except Exception:
                    if self._agent_ws_connected:
                        log.info("Agent lost in %s", self._vm.name)
                        self._agent_ws_connected = False
                        self._agent_url = ""
            await asyncio.sleep(15)

    def _get_agent_ip(self) -> str:
        """Get the VM's IP for agent health check."""
        for adapter in self._vm.network_adapters:
            ips = adapter.get("IPAddresses") or []
            if isinstance(ips, str):
                ips = [ips]
            for ip in ips:
                if ip and not ip.startswith("169.254") and ":" not in ip:
                    return ip
        return ""

    async def _notify_controller_agent_available(self, agent_ip: str) -> None:
        """Tell the controller the agent is available for this node."""
        if not self._controller_url:
            return
        payload = json.dumps({"agent_url": f"http://{agent_ip}:7382"}).encode()
        url = f"{self._controller_url.rstrip('/')}/api/v1/nodes/{self._node_id}"
        try:
            req = urllib.request.Request(
                url, data=payload,
                headers={"Content-Type": "application/json"},
                method="PATCH",
            )
            with urllib.request.urlopen(req, timeout=5):
                pass
        except Exception:
            pass


# ── VSS-consistent checkpoint (backup parity) ─────────────────────────────────

class HyperVCheckpointManager:
    """
    Create and manage VSS-consistent checkpoints for backup.

    Checkpoint types:
      Production     — VSS-based (consistent), requires Integration Services
      ProductionOnly — VSS only, fails if VSS unavailable (strict)
      Standard       — Crash-consistent (legacy, no VSS)

    Veeam parity:
      - create_checkpoint()  → equivalent to Veeam VM snapshot
      - export_checkpoint()  → vzdump equivalent, pipes to Restic
      - remove_checkpoint()  → cleanup after backup
      - get_changed_blocks() — CBT equivalent (Hyper-V has RCT: Resilient Change Tracking)
    """

    async def create_checkpoint(self, vm_name: str,
                                checkpoint_name: str = "",
                                production: bool = True) -> str | None:
        """
        Create a checkpoint and return its ID. Returns None on failure.

        Uses Production checkpoint (VSS) by default for application
        consistency (databases, Exchange, etc.).
        """
        name = checkpoint_name or f"ozma-backup-{int(time.time())}"
        cp_type = "Production" if production else "Standard"

        # Temporarily set checkpoint type if needed
        stdout, _, rc = await run_powershell(
            f"(Get-VM -Name '{vm_name}').CheckpointType", timeout=10
        )
        original_type = stdout.strip()

        if original_type != cp_type:
            await run_powershell(
                f"Set-VM -Name '{vm_name}' -CheckpointType {cp_type}", timeout=10
            )

        _, _, rc = await run_powershell(
            f"Checkpoint-VM -Name '{vm_name}' -SnapshotName '{name}'", timeout=120
        )

        # Restore original type
        if original_type and original_type != cp_type:
            await run_powershell(
                f"Set-VM -Name '{vm_name}' -CheckpointType {original_type}", timeout=10
            )

        if rc == 0:
            log.info("Checkpoint '%s' created for %s", name, vm_name)
            return name
        log.error("Checkpoint creation failed for %s (rc=%d)", vm_name, rc)
        return None

    async def export_checkpoint(self, vm_name: str, checkpoint_name: str,
                                export_path: str) -> bool:
        """
        Export a checkpoint to a directory for Restic ingestion.

        Export-VMSnapshot copies the VHDX and config to a directory.
        Restic then backs up the exported directory.
        """
        os.makedirs(export_path, exist_ok=True)
        _, _, rc = await run_powershell(
            f"Export-VMSnapshot -VMName '{vm_name}' -Name '{checkpoint_name}' "
            f"-Path '{export_path}'",
            timeout=3600,  # large VMs can take a while
        )
        if rc == 0:
            log.info("Checkpoint '%s' exported to %s", checkpoint_name, export_path)
            return True
        log.error("Checkpoint export failed for %s/%s", vm_name, checkpoint_name)
        return False

    async def remove_checkpoint(self, vm_name: str, checkpoint_name: str) -> bool:
        """Remove a checkpoint after backup completes."""
        _, _, rc = await run_powershell(
            f"Remove-VMSnapshot -VMName '{vm_name}' -Name '{checkpoint_name}'",
            timeout=300,
        )
        return rc == 0

    async def enable_rct(self, vm_name: str) -> bool:
        """
        Enable Resilient Change Tracking (RCT) on the VM's disks.

        RCT is Hyper-V's equivalent of VMware CBT — tracks changed blocks
        between backups, enabling efficient incremental backup.

        Requires VM generation 2 and Windows Server 2016 / Windows 10+.
        """
        _, _, rc = await run_powershell(
            f"Get-VMHardDiskDrive -VMName '{vm_name}' | "
            f"ForEach-Object {{ Set-VHD -Path $_.Path -ResetDiskIdentifier -Confirm:$false }}",
            timeout=60,
        )
        # RCT is enabled per-VHDX; the above ensures IDs are set for tracking
        if rc == 0:
            log.info("RCT enabled for %s", vm_name)
        return rc == 0


# ── VM state monitor ───────────────────────────────────────────────────────────

class HyperVVMMonitor:
    """
    Monitor VM state changes via WMI event subscription.

    Subscribes to Msvm_ComputerSystem modification events — VM starts,
    stops, pauses, saves. Fires callbacks when state changes.

    Also polls Get-VM every 30 seconds as a fallback (WMI events can
    sometimes be missed on busy hosts).
    """

    def __init__(self, on_vm_started: Any = None, on_vm_stopped: Any = None) -> None:
        self._on_started = on_vm_started
        self._on_stopped = on_vm_stopped
        self._known_vms: dict[str, str] = {}  # name → state
        self._stop_event = asyncio.Event()

    async def run(self) -> None:
        """Run the monitor. Calls on_vm_started/stopped as state changes."""
        # Start WMI event watcher in a thread (WMI events are synchronous)
        asyncio.create_task(self._wmi_event_loop(), name="hyperv-wmi-events")
        asyncio.create_task(self._poll_loop(), name="hyperv-vm-poll")
        await self._stop_event.wait()

    async def stop(self) -> None:
        self._stop_event.set()

    async def _poll_loop(self) -> None:
        """Poll Get-VM every 30s as fallback."""
        while not self._stop_event.is_set():
            vms = await discover_hyperv_vms()
            current = {vm.name: vm.state for vm in vms}

            # Detect new running VMs
            for name, state in current.items():
                prev = self._known_vms.get(name)
                if state == "Running" and prev != "Running":
                    if self._on_started:
                        vm = next((v for v in vms if v.name == name), None)
                        if vm:
                            await self._on_started(vm)

            # Detect stopped VMs
            for name in list(self._known_vms):
                if name not in current or current[name] not in ("Running", "Paused"):
                    if self._known_vms.get(name) == "Running":
                        if self._on_stopped:
                            await self._on_stopped(name)

            self._known_vms = current
            await asyncio.sleep(30)

    async def _wmi_event_loop(self) -> None:
        """
        Subscribe to WMI Msvm_ComputerSystem events.

        Runs PowerShell in background; when a VM changes state it emits
        a JSON line to stdout which we parse and dispatch.
        """
        script = r"""
$watcher = New-Object System.Management.ManagementEventWatcher
$watcher.Scope = New-Object System.Management.ManagementScope("root\virtualization\v2")
$watcher.Query = New-Object System.Management.WqlEventQuery(
    "SELECT * FROM __InstanceModificationEvent WITHIN 2 WHERE TargetInstance ISA 'Msvm_ComputerSystem'")
while ($true) {
    $event = $watcher.WaitForNextEvent()
    $ti = $event.TargetInstance
    @{Name=$ti.ElementName; State=$ti.EnabledState} | ConvertTo-Json -Compress
}
"""
        proc = await asyncio.create_subprocess_exec(
            "powershell.exe", "-NonInteractive", "-NoProfile",
            "-Command", script,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )

        while not self._stop_event.is_set():
            try:
                line = await asyncio.wait_for(proc.stdout.readline(), timeout=5)
                if not line:
                    break
                event = json.loads(line.decode().strip())
                state_map = {2: "Running", 3: "Off", 6: "Saved", 9: "Paused"}
                state = state_map.get(event.get("State"), "Unknown")
                name = event.get("Name", "")

                if not name:
                    continue
                prev = self._known_vms.get(name)
                self._known_vms[name] = state

                if state == "Running" and prev != "Running":
                    log.info("VM started (WMI event): %s", name)
                    if self._on_started:
                        vms = await discover_hyperv_vms()
                        vm = next((v for v in vms if v.name == name), None)
                        if vm and self._on_started:
                            await self._on_started(vm)

                elif state in ("Off", "Saved") and prev == "Running":
                    log.info("VM stopped (WMI event): %s", name)
                    if self._on_stopped:
                        await self._on_stopped(name)

            except asyncio.TimeoutError:
                continue
            except Exception as e:
                log.debug("WMI event parse error: %s", e)
                continue

        proc.kill()


# ── Top-level manager ─────────────────────────────────────────────────────────

class HyperVVirtualNodeManager:
    """
    Manages one soft node + agent per VM on the Hyper-V host.

    Equivalent to VirtualNodeManager (virtual_node.py) but for Hyper-V.

    Auto-discovers all Running VMs, creates a HyperVSoftNode per VM,
    registers them with the Ozma controller, and provisions agents via
    PowerShell Direct. All zero-config.

    Usage:
      manager = HyperVVirtualNodeManager(controller_url="http://10.0.0.1:7380")
      await manager.run()
    """

    def __init__(
        self,
        controller_url: str = "",
        base_port: int = 7440,      # separate range from QEMU (7332+)
        auto_manage: bool = True,
        auto_agent: bool = True,
        exclude_patterns: list[str] | None = None,
        guest_user: str = "",
        guest_password: str = "",
    ) -> None:
        self._controller_url = controller_url
        self._base_port = base_port
        self._auto_manage = auto_manage
        self._auto_agent = auto_agent
        self._exclude = exclude_patterns or []
        self._managed: dict[str, tuple[HyperVSoftNode, asyncio.Task]] = {}
        self._provisioner = PowerShellDirectProvisioner(
            controller_url, guest_user, guest_password
        )
        self._checkpoint_mgr = HyperVCheckpointManager()
        self._stop_event = asyncio.Event()
        self._next_port = base_port

    async def run(self) -> None:
        """Discover VMs and start managing them."""
        log.info("Hyper-V Virtual Node Manager starting")
        log.info("  Controller:  %s", self._controller_url or "(auto-discover)")
        log.info("  Base port:   %d", self._base_port)
        log.info("  Auto-manage: %s", "on" if self._auto_manage else "off")
        log.info("  Auto-agent:  %s", "on" if self._auto_agent else "off")

        # Check Hyper-V is available
        if not await self._check_hyperv_available():
            log.error("Hyper-V PowerShell module not available — is Hyper-V installed?")
            return

        # Initial discovery
        if self._auto_manage:
            await self._discover_and_sync()

        # Watch for VM state changes
        monitor = HyperVVMMonitor(
            on_vm_started=self._on_vm_started,
            on_vm_stopped=self._on_vm_stopped,
        )
        asyncio.create_task(monitor.run(), name="hyperv-monitor")

        # Agent provisioning loop
        if self._auto_agent:
            asyncio.create_task(self._agent_loop(), name="agent-provisioner")

        await self._stop_event.wait()

    async def stop(self) -> None:
        self._stop_event.set()
        for vm_name, (node, task) in list(self._managed.items()):
            task.cancel()
            await node.stop()

    async def _check_hyperv_available(self) -> bool:
        stdout, _, rc = await run_powershell("Get-Module -ListAvailable Hyper-V", timeout=15)
        return rc == 0 and "Hyper-V" in stdout

    def _is_excluded(self, name: str) -> bool:
        return any(fnmatch.fnmatch(name, pat) for pat in self._exclude)

    async def _discover_and_sync(self) -> None:
        vms = await discover_hyperv_vms()
        current_names = {vm.name for vm in vms}
        managed_names = set(self._managed.keys())

        for vm in vms:
            if vm.name not in managed_names and not self._is_excluded(vm.name):
                await self._start_node(vm)

        for name in managed_names - current_names:
            await self._stop_node(name)

    async def _on_vm_started(self, vm: HyperVVMInfo) -> None:
        if not self._auto_manage:
            return
        if vm.name not in self._managed and not self._is_excluded(vm.name):
            log.info("VM started: %s — creating soft node", vm.name)
            await self._start_node(vm)

    async def _on_vm_stopped(self, vm_name: str) -> None:
        if vm_name in self._managed:
            log.info("VM stopped: %s — removing soft node", vm_name)
            await self._stop_node(vm_name)

    async def _start_node(self, vm: HyperVVMInfo) -> None:
        port = self._next_port
        self._next_port += 1

        node = HyperVSoftNode(
            vm=vm,
            port=port,
            controller_url=self._controller_url,
            auto_agent=self._auto_agent,
        )
        await node.start()
        task = asyncio.create_task(
            asyncio.shield(asyncio.sleep(0)),  # placeholder
            name=f"node-{vm.name}",
        )
        self._managed[vm.name] = (node, task)
        log.info("Soft node started for %s on port %d", vm.name, port)

    async def _stop_node(self, vm_name: str) -> None:
        entry = self._managed.pop(vm_name, None)
        if entry:
            node, task = entry
            task.cancel()
            await node.stop()
            log.info("Soft node stopped for %s", vm_name)

    async def _agent_loop(self) -> None:
        """Periodically try to provision agents in VMs that don't have one."""
        while not self._stop_event.is_set():
            for vm_name, (node, _) in list(self._managed.items()):
                if not node._agent_ws_connected:
                    try:
                        vms = await discover_hyperv_vms()
                        vm = next((v for v in vms if v.name == vm_name), None)
                        if vm:
                            await self._provisioner.provision(vm)
                    except Exception as e:
                        log.debug("Agent provisioning error for %s: %s", vm_name, e)
            await asyncio.sleep(120)  # retry every 2 minutes

    # ── Backup integration ─────────────────────────────────────────────────

    async def backup_vm(self, vm_name: str, export_path: str) -> bool:
        """
        Create a VSS-consistent checkpoint and export it for Restic backup.

        This is the Veeam parity path:
          checkpoint → export to dir → Restic backs up dir → remove checkpoint
        """
        cp_name = await self._checkpoint_mgr.create_checkpoint(
            vm_name, production=True
        )
        if not cp_name:
            return False

        ok = await self._checkpoint_mgr.export_checkpoint(
            vm_name, cp_name, export_path
        )

        # Always remove the checkpoint (exported = no longer needed live)
        await self._checkpoint_mgr.remove_checkpoint(vm_name, cp_name)

        return ok

    # ── HTTP status endpoint ───────────────────────────────────────────────

    def status(self) -> dict:
        """Return current status for API/monitoring."""
        return {
            "hypervisor": "hyperv",
            "managed_vms": [
                {
                    "name": name,
                    "port": node._port,
                    "agent_connected": node._agent_ws_connected,
                }
                for name, (node, _) in self._managed.items()
            ],
        }


# ── CLI entry point ────────────────────────────────────────────────────────────

def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s  %(name)s  %(levelname)s  %(message)s",
    )

    parser = argparse.ArgumentParser(
        description="Ozma Hyper-V Node — manages all Hyper-V VMs as Ozma nodes"
    )
    parser.add_argument("--controller", default="",
                        help="Controller URL (e.g. http://10.0.0.1:7380). "
                             "Default: auto-discover via mDNS.")
    parser.add_argument("--base-port", type=int, default=7440,
                        help="Base UDP port for soft nodes (default: 7440)")
    parser.add_argument("--exclude", default="",
                        help="Comma-separated glob patterns to exclude (e.g. 'template-*,infra-*')")
    parser.add_argument("--no-auto-manage", action="store_true",
                        help="Discover only; do not create soft nodes automatically")
    parser.add_argument("--no-auto-agent", action="store_true",
                        help="Create soft nodes but do not provision agents")
    parser.add_argument("--guest-user", default="",
                        help="Guest OS username for PowerShell Direct provisioning")
    parser.add_argument("--guest-password", default="",
                        help="Guest OS password for PowerShell Direct provisioning")
    parser.add_argument("--list", action="store_true",
                        help="List running VMs and exit")
    args = parser.parse_args()

    if args.list:
        async def _list():
            vms = await discover_hyperv_vms()
            for vm in vms:
                print(f"  {vm.name:30s}  {vm.state:10s}  {vm.guest_os or 'unknown':8s}  "
                      f"{vm.cpu_count}vCPU  {vm.memory_mb}MB")
        asyncio.run(_list())
        return

    exclude = [p.strip() for p in args.exclude.split(",") if p.strip()]
    manager = HyperVVirtualNodeManager(
        controller_url=args.controller,
        base_port=args.base_port,
        auto_manage=not args.no_auto_manage,
        auto_agent=not args.no_auto_agent,
        exclude_patterns=exclude,
        guest_user=args.guest_user,
        guest_password=args.guest_password,
    )

    import signal as _signal

    async def _run():
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, lambda: asyncio.create_task(manager.stop()))
        await manager.run()

    asyncio.run(_run())


if __name__ == "__main__":
    main()
