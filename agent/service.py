# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Cross-platform background service management for the ozma agent.

Usage:
  ozma-agent install --name my-pc --controller https://ozma.hrdwrbob.net
  ozma-agent uninstall
  ozma-agent status

Linux:   systemd user service (~/.config/systemd/user/ozma-agent.service)
macOS:   launchd LaunchAgent (~/Library/LaunchAgents/com.ozmalabs.agent.plist)
Windows: Task Scheduler task (runs at logon, restarts on failure)
         Or NSSM service if nssm.exe is available
"""

from __future__ import annotations

import logging
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

log = logging.getLogger("ozma.agent.service")


def install_service(name: str, controller_url: str) -> bool:
    """Install the agent as a background service that starts on boot."""
    system = platform.system()
    agent_cmd = _find_agent_command()

    if system == "Linux":
        return _install_systemd(name, controller_url, agent_cmd)
    elif system == "Darwin":
        return _install_launchd(name, controller_url, agent_cmd)
    elif system == "Windows":
        return _install_windows(name, controller_url, agent_cmd)
    else:
        log.error("Unsupported platform: %s", system)
        return False


def uninstall_service() -> bool:
    """Remove the agent background service."""
    system = platform.system()

    if system == "Linux":
        return _uninstall_systemd()
    elif system == "Darwin":
        return _uninstall_launchd()
    elif system == "Windows":
        return _uninstall_windows()
    else:
        log.error("Unsupported platform: %s", system)
        return False


def service_status() -> dict:
    """Check the agent service status."""
    system = platform.system()
    result = {"platform": system, "installed": False, "running": False}

    if system == "Linux":
        svc = Path.home() / ".config/systemd/user/ozma-agent.service"
        result["installed"] = svc.exists()
        if result["installed"]:
            r = subprocess.run(
                ["systemctl", "--user", "is-active", "ozma-agent"],
                capture_output=True, text=True,
            )
            result["running"] = r.stdout.strip() == "active"

    elif system == "Darwin":
        plist = Path.home() / "Library/LaunchAgents/com.ozmalabs.agent.plist"
        result["installed"] = plist.exists()
        if result["installed"]:
            r = subprocess.run(
                ["launchctl", "list", "com.ozmalabs.agent"],
                capture_output=True, text=True,
            )
            result["running"] = r.returncode == 0

    elif system == "Windows":
        r = subprocess.run(
            ["schtasks", "/Query", "/TN", "OzmaAgent"],
            capture_output=True, text=True,
        )
        result["installed"] = r.returncode == 0
        result["running"] = "Running" in r.stdout if r.returncode == 0 else False

    return result


# ── Linux (systemd) ───────────────────────────────────────────────────────

def _install_systemd(name: str, controller_url: str, agent_cmd: str) -> bool:
    svc_dir = Path.home() / ".config/systemd/user"
    svc_dir.mkdir(parents=True, exist_ok=True)
    svc_path = svc_dir / "ozma-agent.service"

    unit = f"""[Unit]
Description=Ozma Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={agent_cmd} --name {name} --controller {controller_url}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"""
    svc_path.write_text(unit)
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    subprocess.run(["systemctl", "--user", "enable", "ozma-agent"], check=True)
    subprocess.run(["systemctl", "--user", "start", "ozma-agent"], check=True)
    # Enable lingering so the service runs without a login session
    subprocess.run(["loginctl", "enable-linger", os.environ.get("USER", "")],
                   capture_output=True)
    log.info("Installed systemd service: %s", svc_path)
    return True


def _uninstall_systemd() -> bool:
    subprocess.run(["systemctl", "--user", "stop", "ozma-agent"], capture_output=True)
    subprocess.run(["systemctl", "--user", "disable", "ozma-agent"], capture_output=True)
    svc_path = Path.home() / ".config/systemd/user/ozma-agent.service"
    if svc_path.exists():
        svc_path.unlink()
    subprocess.run(["systemctl", "--user", "daemon-reload"], capture_output=True)
    log.info("Uninstalled systemd service")
    return True


# ── macOS (launchd) ───────────────────────────────────────────────────────

def _install_launchd(name: str, controller_url: str, agent_cmd: str) -> bool:
    plist_dir = Path.home() / "Library/LaunchAgents"
    plist_dir.mkdir(parents=True, exist_ok=True)
    plist_path = plist_dir / "com.ozmalabs.agent.plist"

    # Split agent_cmd into program and args
    parts = agent_cmd.split()
    program = parts[0]
    args_xml = "\n".join(f"    <string>{a}</string>" for a in parts)

    plist = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ozmalabs.agent</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}
        <string>--name</string>
        <string>{name}</string>
        <string>--controller</string>
        <string>{controller_url}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/ozma-agent.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/ozma-agent.log</string>
</dict>
</plist>
"""
    plist_path.write_text(plist)
    subprocess.run(["launchctl", "load", str(plist_path)], check=True)
    log.info("Installed launchd agent: %s", plist_path)
    return True


def _uninstall_launchd() -> bool:
    plist_path = Path.home() / "Library/LaunchAgents/com.ozmalabs.agent.plist"
    if plist_path.exists():
        subprocess.run(["launchctl", "unload", str(plist_path)], capture_output=True)
        plist_path.unlink()
    log.info("Uninstalled launchd agent")
    return True


# ── Windows (Task Scheduler / NSSM) ──────────────────────────────────────

def _install_windows(name: str, controller_url: str, agent_cmd: str) -> bool:
    # Try NSSM first (proper Windows service)
    nssm = shutil.which("nssm")
    if not nssm:
        # Check alongside the agent exe
        agent_dir = Path(sys.executable).parent if getattr(sys, 'frozen', False) else Path(__file__).parent
        nssm_path = agent_dir / "nssm.exe"
        if nssm_path.exists():
            nssm = str(nssm_path)

    if nssm:
        subprocess.run([nssm, "install", "OzmaAgent", agent_cmd,
                        "--name", name, "--controller", controller_url], check=True)
        subprocess.run([nssm, "set", "OzmaAgent", "Description",
                        "Ozma Agent — connects this machine to your ozma mesh"], capture_output=True)
        subprocess.run([nssm, "set", "OzmaAgent", "Start", "SERVICE_AUTO_START"], capture_output=True)
        subprocess.run([nssm, "start", "OzmaAgent"], capture_output=True)
        log.info("Installed Windows service via NSSM")
        return True

    # Fallback: Task Scheduler
    cmd = f'"{agent_cmd}" --name "{name}" --controller "{controller_url}"'
    subprocess.run([
        "schtasks", "/Create", "/TN", "OzmaAgent",
        "/TR", cmd,
        "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
    ], check=True)
    subprocess.run(["schtasks", "/Run", "/TN", "OzmaAgent"], capture_output=True)
    log.info("Installed Windows scheduled task")
    return True


def _uninstall_windows() -> bool:
    # Try NSSM
    nssm = shutil.which("nssm")
    if nssm:
        subprocess.run([nssm, "stop", "OzmaAgent"], capture_output=True)
        subprocess.run([nssm, "remove", "OzmaAgent", "confirm"], capture_output=True)
    # Also remove scheduled task
    subprocess.run(["schtasks", "/Delete", "/TN", "OzmaAgent", "/F"], capture_output=True)
    log.info("Uninstalled Windows service/task")
    return True


# ── Helpers ───────────────────────────────────────────────────────────────

def _find_agent_command() -> str:
    """Find the agent executable/script path."""
    # If running as a frozen PyInstaller exe
    if getattr(sys, 'frozen', False):
        return sys.executable

    # If installed via pip
    agent_bin = shutil.which("ozma-agent")
    if agent_bin:
        return agent_bin

    # Fallback: run the module
    return f"{sys.executable} -m ozma_desktop_agent"
