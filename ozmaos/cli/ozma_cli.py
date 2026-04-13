#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
OzmaOS Management CLI — `ozma` command for OS-level operations.

Usage:
    ozma status
    ozma update [--check] [--apply]
    ozma service enable|disable|restart <name>
    ozma connect link <email>|unlink|status
    ozma logs [service]
    ozma backup now
    ozma zfs status
    ozma diagnose
"""

from __future__ import annotations

import asyncio
import gzip
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Optional

import click

# ── Constants ────────────────────────────────────────────────────────────────

CONTROLLER_API = "http://localhost:7380"
MANAGED_SERVICES = [
    "frigate",
    "vaultwarden",
    "parsec",
    "tailscale",
    "nginx",
    "ozma-controller",
    "ozma-node",
]


# ── Helpers ─────────────────────────────────────────────────────────────────

def run_cmd(cmd: list[str], check: bool = False) -> tuple[int, str, str]:
    """Run a command, return (returncode, stdout, stderr)."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if check and result.returncode != 0:
            raise click.ClickException(f"Command failed: {' '.join(cmd)}")
        return result.returncode, result.stdout.strip(), result.stderr.strip()
    except subprocess.TimeoutExpired:
        return -1, "", "Command timed out"
    except FileNotFoundError:
        return -1, "", f"Command not found: {cmd[0]}"


def get_controller_status() -> dict:
    """Fetch status from local controller API."""
    try:
        import urllib.request
        req = urllib.request.Request(f"{CONTROLLER_API}/api/v1/status")
        with urllib.request.urlopen(req, timeout=5) as r:
            return json.loads(r.read())
    except Exception:
        return {}


def is_docker_service_running(name: str) -> bool:
    """Check if a Docker Compose service is running."""
    rc, out, _ = run_cmd(["docker", "compose", "ps", "--format", "json", name])
    if rc == 0 and out:
        try:
            for line in out.splitlines():
                data = json.loads(line)
                if data.get("Service") == name:
                    return data.get("State") == "running"
        except json.JSONDecodeError:
            pass
    return False


def is_systemd_active(unit: str) -> bool:
    """Check if a systemd unit is active."""
    rc, out, _ = run_cmd(["systemctl", "is-active", unit])
    return rc == 0 and out == "active"


def get_wireguard_status() -> tuple[bool, str]:
    """Get WireGuard interface status."""
    rc, out, _ = run_cmd(["wg", "show"])
    if rc == 0 and out:
        return True, "active"
    return False, "inactive"


def get_network_info() -> dict:
    """Get network information."""
    info = {"ip": "unknown", "wg": "inactive"}
    
    # Get primary IP
    rc, out, _ = run_cmd(["ip", "route", "get", "1.1.1.1"])
    if rc == 0:
        match = re.search(r"src (\S+)", out)
        if match:
            info["ip"] = match.group(1)
    
    # WireGuard status
    info["wg"], _ = get_wireguard_status()
    
    return info


def get_zfs_status() -> list[dict]:
    """Get ZFS pool status."""
    pools = []
    rc, out, _ = run_cmd(["zpool", "list", "-H", "-o", "name,size,alloc,free,health"])
    if rc == 0:
        for line in out.splitlines():
            parts = line.split("\t")
            if len(parts) >= 5:
                pools.append({
                    "name": parts[0],
                    "size": parts[1],
                    "alloc": parts[2],
                    "free": parts[3],
                    "health": parts[4],
                })
    return pools


def get_disk_usage(paths: list[str] = None) -> list[dict]:
    """Get disk usage for specified paths."""
    if paths is None:
        paths = ["/", "/var/ozma", "/home"]
    
    usage = []
    for path in paths:
        try:
            import shutil as sh
            stat = shutil.disk_usage(path)
            usage.append({
                "path": path,
                "total": f"{stat.total / (1024**3):.1f}G",
                "used": f"{stat.used / (1024**3):.1f}G",
                "free": f"{stat.free / (1024**3):.1f}G",
                "percent": int(stat.used / stat.total * 100),
            })
        except Exception:
            pass
    return usage


# ── CLI Commands ─────────────────────────────────────────────────────────────

@click.group()
@click.version_option(version="1.0.0")
def cli():
    """OzmaOS management CLI."""
    pass


@cli.command()
def status():
    """Show system health summary."""
    click.echo(click.style("═══ Ozma System Status ═══", fg="cyan", bold=True))
    click.echo()
    
    # Controller status
    click.echo(click.style("Controller:", bold=True))
    if is_systemd_active("ozma-controller.service"):
        version = "unknown"
        try:
            status_data = get_controller_status()
            version = status_data.get("version", "unknown")
        except Exception:
            pass
        click.echo(f"  ● Running (v{version})")
    else:
        click.echo("  ○ Stopped")
    click.echo()
    
    # Node daemon status
    click.echo(click.style("Node Daemon:", bold=True))
    if is_systemd_active("ozma-node.service"):
        click.echo("  ● Running")
    else:
        click.echo("  ○ Stopped")
    click.echo()
    
    # Managed services
    click.echo(click.style("Services:", bold=True))
    for svc in MANAGED_SERVICES:
        # Check systemd first, then docker
        active = is_systemd_active(f"{svc}.service")
        if not active:
            active = is_docker_service_running(svc)
        status_str = click.style("●", fg="green") if active else click.style("○", fg="red")
        click.echo(f"  {status_str} {svc}")
    click.echo()
    
    # Network
    click.echo(click.style("Network:", bold=True))
    net = get_network_info()
    click.echo(f"  IP: {net['ip']}")
    wg_color = "green" if net["wg"] else "red"
    wg_status = "active" if net["wg"] else "inactive"
    click.echo(f"  WireGuard: {click.style(wg_status, fg=wg_color)}")
    
    # Connect status
    try:
        status_data = get_controller_status()
        if status_data.get("connect", {}).get("authenticated"):
            click.echo(f"  Connect: {click.style('linked', fg='green')} "
                       f"({status_data['connect'].get('tier', 'free')})")
        else:
            click.echo(f"  Connect: {click.style('not linked', fg='yellow')}")
    except Exception:
        click.echo("  Connect: unknown")
    click.echo()
    
    # ZFS
    click.echo(click.style("ZFS Pools:", bold=True))
    pools = get_zfs_status()
    if pools:
        for pool in pools:
            health_color = "green" if pool["health"] == "ONLINE" else "red"
            click.echo(f"  {pool['name']}: {pool['health']} "
                       f"({pool['alloc']} / {pool['size']})")
    else:
        click.echo("  No ZFS pools found")
    click.echo()
    
    # Storage
    click.echo(click.style("Storage:", bold=True))
    for du in get_disk_usage():
        bar = "█" * (du["percent"] // 5) + "░" * (20 - du["percent"] // 5)
        color = "green" if du["percent"] < 80 else "yellow" if du["percent"] < 90 else "red"
        click.echo(f"  {du['path']}: {click.style(bar, fg=color)} {du['percent']}% "
                   f"({du['used']} / {du['total']})")


@cli.command()
@click.option("--check", is_flag=True, help="Check for available updates")
@click.option("--apply", is_flag=True, help="Apply updates")
def update(check: bool, apply: bool):
    """Check for and apply OS/Controller updates."""
    if not check and not apply:
        check = True
    
    click.echo(click.style("═══ Ozma Update Manager ═══", fg="cyan", bold=True))
    click.echo()
    
    # Controller version
    click.echo(click.style("Controller:", bold=True))
    current_version = "unknown"
    try:
        status_data = get_controller_status()
        current_version = status_data.get("version", "unknown")
    except Exception:
        pass
    click.echo(f"  Current: v{current_version}")
    
    if check or apply:
        # Check for controller updates
        available_version = "1.0.1"  # Placeholder - would query update server
        click.echo(f"  Available: v{available_version}")
        
        if current_version != available_version:
            click.echo(click.style("  Update available!", fg="yellow"))
        else:
            click.echo(click.style("  Up to date", fg="green"))
    click.echo()
    
    # OS packages
    click.echo(click.style("OS Packages:", bold=True))
    rc, out, _ = run_cmd(["apt", "list", "--upgradable", "2>/dev/null"])
    if rc == 0 and out and out != "Listing...":
        upgrades = len(out.splitlines())
        click.echo(f"  {upgrades} package(s) can be upgraded")
        if check:
            click.echo(click.style("  Run with --apply to upgrade", fg="yellow"))
    else:
        click.echo("  All packages up to date")
    click.echo()
    
    if apply:
        if not click.confirm("Apply updates?"):
            return
        
        click.echo(click.style("Applying updates...", fg="yellow"))
        
        # Controller OTA update (placeholder)
        click.echo("  Updating controller...")
        # In production: call controller update API or subprocess
        
        # OS package update
        click.echo("  Updating OS packages...")
        rc, _, err = run_cmd(["apt", "update"])
        if rc != 0:
            click.echo(click.style(f"  apt update failed: {err}", fg="red"))
            return
        
        rc, _, err = run_cmd(["apt", "upgrade", "--security", "-y"])
        if rc != 0:
            click.echo(click.style(f"  apt upgrade failed: {err}", fg="red"))
            return
        
        click.echo(click.style("  Updates applied successfully!", fg="green"))
        
        # Restart controller if needed
        if click.confirm("Restart controller to apply changes?"):
            run_cmd(["systemctl", "restart", "ozma-controller"])


@cli.group(name="service")
def service_group():
    """Manage optional services."""
    pass


@service_group.command(name="enable")
@click.argument("name")
def service_enable(name: str):
    """Enable a managed service."""
    name = name.lower()
    if name not in MANAGED_SERVICES:
        click.echo(click.style(f"Unknown service: {name}", fg="red"))
        click.echo(f"Available: {', '.join(MANAGED_SERVICES)}")
        raise SystemExit(1)
    
    # Enable and start systemd unit if exists
    run_cmd(["systemctl", "enable", f"{name}.service"], check=False)
    rc, _, err = run_cmd(["systemctl", "start", f"{name}.service"])
    
    if rc == 0:
        click.echo(click.style(f"Service '{name}' enabled and started", fg="green"))
    else:
        click.echo(click.style(f"Failed to start {name}: {err}", fg="red"))


@service_group.command(name="disable")
@click.argument("name")
def service_disable(name: str):
    """Disable a managed service."""
    name = name.lower()
    if name not in MANAGED_SERVICES:
        click.echo(click.style(f"Unknown service: {name}", fg="red"))
        click.echo(f"Available: {', '.join(MANAGED_SERVICES)}")
        raise SystemExit(1)
    
    run_cmd(["systemctl", "stop", f"{name}.service"], check=False)
    run_cmd(["systemctl", "disable", f"{name}.service"], check=False)
    click.echo(click.style(f"Service '{name}' disabled and stopped", fg="yellow"))


@service_group.command(name="restart")
@click.argument("name")
def service_restart(name: str):
    """Restart a managed service."""
    name = name.lower()
    if name not in MANAGED_SERVICES:
        click.echo(click.style(f"Unknown service: {name}", fg="red"))
        click.echo(f"Available: {', '.join(MANAGED_SERVICES)}")
        raise SystemExit(1)
    
    rc, _, err = run_cmd(["systemctl", "restart", f"{name}.service"])
    if rc == 0:
        click.echo(click.style(f"Service '{name}' restarted", fg="green"))
    else:
        click.echo(click.style(f"Failed to restart {name}: {err}", fg="red"))


@cli.group(name="connect")
def connect_group():
    """Manage Ozma Connect integration."""
    pass


@connect_group.command(name="link")
@click.argument("email")
def connect_link(email: str):
    """Link controller to Ozma Connect account."""
    click.echo(f"Linking to Ozma Connect as {email}...")
    
    # Get password securely
    password = click.prompt("Password", hide_input=True)
    
    async def do_login():
        # Import here to avoid circular imports
        from controller.connect import OzmaConnect
        client = OzmaConnect()
        success = await client.login(email, password)
        if success:
            click.echo(click.style("Successfully linked to Ozma Connect!", fg="green"))
            click.echo(f"Tier: {client.tier}")
        else:
            click.echo(click.style("Failed to link. Check credentials.", fg="red"))
            raise SystemExit(1)
    
    asyncio.run(do_login())


@connect_group.command(name="unlink")
def connect_unlink():
    """Unlink controller from Ozma Connect."""
    if not click.confirm("Unlink from Ozma Connect?"):
        return
    
    async def do_logout():
        from controller.connect import OzmaConnect
        client = OzmaConnect()
        client.logout()
        click.echo(click.style("Unlinked from Ozma Connect", fg="yellow"))
    
    asyncio.run(do_logout())


@connect_group.command(name="status")
def connect_status():
    """Show Connect relay and backup status."""
    click.echo(click.style("═══ Ozma Connect Status ═══", fg="cyan", bold=True))
    click.echo()
    
    async def do_status():
        from controller.connect import OzmaConnect
        client = OzmaConnect()
        await client.start()
        
        if not client.authenticated:
            click.echo(click.style("Not linked to Ozma Connect", fg="yellow"))
            return
        
        click.echo(f"Account ID: {client.account_id}")
        click.echo(f"Tier: {click.style(client.tier, fg='green' if client.tier != 'free' else 'yellow')}")
        click.echo()
        
        # Relay status
        relay_config = client.relay_config
        if relay_config:
            click.echo(click.style("Relay:", bold=True))
            click.echo(f"  Controller ID: {client.relay_controller_id}")
            click.echo(f"  Zone: {relay_config.get('zone', 'unknown')}")
            click.echo(f"  Mesh IP: {relay_config.get('mesh_ip', 'unknown')}")
            click.echo(f"  Endpoint: {relay_config.get('relay_endpoint', 'unknown')}")
        else:
            click.echo(click.style("Relay: Not configured", fg="yellow"))
        
        click.echo()
        
        # Backup status
        click.echo(click.style("Backup:", bold=True))
        backup_path = Path("/var/ozma/backups")
        if backup_path.exists():
            backups = sorted(backup_path.glob("*.enc"), key=lambda p: p.stat().st_mtime, reverse=True)
            if backups:
                latest = backups[0]
                age = datetime.now() - datetime.fromtimestamp(latest.stat().st_mtime)
                if age.days == 0:
                    age_str = "today"
                else:
                    age_str = f"{age.days} days ago"
                click.echo(f"  Latest: {latest.name} ({age_str})")
            else:
                click.echo("  No backups found")
        else:
            click.echo("  No backup storage configured")
    
    asyncio.run(do_status())


@cli.command()
@click.argument("service", required=False, default="ozma-controller")
@click.option("-n", "--lines", default=50, help="Number of lines to show")
@click.option("-f", "--follow", is_flag=True, help="Follow log output")
def logs(service: str, lines: int, follow: bool):
    """Tail logs for a service (defaults to ozma-controller)."""
    click.echo(click.style(f"Showing logs for {service}...", fg="cyan"))
    
    if is_docker_service_running(service):
        # Docker Compose logs
        cmd = ["docker", "compose", "-f", f"/etc/ozma/{service}/docker-compose.yml", "logs"]
        if follow:
            cmd.append("-f")
        if lines:
            cmd.extend(["--tail", str(lines)])
        subprocess.run(cmd)
    else:
        # Systemd journal
        cmd = ["journalctl", "-u", f"{service}.service", "-n", str(lines)]
        if follow:
            cmd.append("-f")
        subprocess.run(cmd)


@cli.command()
def backup():
    """Trigger immediate backup to Ozma Connect."""
    click.echo(click.style("═══ Ozma Backup ═══", fg="cyan", bold=True))
    click.echo()
    
    async def do_backup():
        from controller.connect import OzmaConnect
        from controller.state import get_state
        
        client = OzmaConnect()
        await client.start()
        
        if not client.authenticated:
            click.echo(click.style("Not linked to Connect. Run 'ozma connect link' first.", fg="red"))
            return
        
        click.echo("Gathering configuration...")
        state = get_state()
        scenarios = state.scenarios.list() if hasattr(state, "scenarios") else []
        
        click.echo("Encrypting and uploading backup...")
        passphrase = click.prompt("Encryption passphrase (leave empty to skip encryption)", 
                                   default="", show_default=False)
        
        # Gather mesh registry
        mesh_registry = {}
        # Would populate from actual mesh registry
        
        success = await client.backup_config(mesh_registry, scenarios, passphrase)
        if success:
            click.echo(click.style("Backup completed successfully!", fg="green"))
        else:
            click.echo(click.style("Backup failed.", fg="red"))
    
    asyncio.run(do_backup())


@cli.command()
def zfs():
    """Show ZFS pool health and scrub status."""
    click.echo(click.style("═══ ZFS Status ═══", fg="cyan", bold=True))
    click.echo()
    
    pools = get_zfs_status()
    if not pools:
        click.echo("No ZFS pools found.")
        return
    
    for pool in pools:
        health_color = "green" if pool["health"] == "ONLINE" else "red"
        click.echo(click.style(f"Pool: {pool['name']}", bold=True))
        click.echo(f"  Health: {click.style(pool['health'], fg=health_color)}")
        click.echo(f"  Size: {pool['size']}")
        click.echo(f"  Used: {pool['alloc']}")
        click.echo(f"  Free: {pool['free']}")
        click.echo()
        
        # Scrub status
        rc, out, _ = run_cmd(["zpool", "status", pool["name"]])
        if rc == 0:
            scrub_match = re.search(r"scan: scrub performed on .*?, (\\d+\\.\\d+% done)", out)
            if scrub_match:
                click.echo(f"  Scrub: {scrub_match.group(1)}")
            else:
                last_scrub = re.search(r"scan: (.*?)$", out, re.MULTILINE)
                if last_scrub:
                    click.echo(f"  Last scrub: {last_scrub.group(1).strip()}")
        click.echo()


@cli.command()
@click.option("-o", "--output", type=click.Path(), help="Output file (default: ozma-diagnose.tar.gz)")
def diagnose(output: Optional[str]):
    """Run health checks and create support bundle."""
    click.echo(click.style("═══ Ozma Diagnose ═══", fg="cyan", bold=True))
    click.echo()
    
    if not output:
        output = f"ozma-diagnose-{datetime.now().strftime('%Y%m%d-%H%M%S')}.tar.gz"
    
    with tempfile.TemporaryDirectory() as tmpdir:
        tmppath = Path(tmpdir)
        
        # Collect system info
        click.echo("Collecting system information...")
        
        # Status output
        status_file = tmppath / "status.txt"
        status_file.write_text(_get_full_status())
        
        # Journal logs
        click.echo("Collecting logs...")
        journal_file = tmppath / "journal.log"
        _, journal_out, _ = run_cmd(["journalctl", "-u", "ozma-controller", "-u", "ozma-node", 
                                     "--no-pager", "-n", "1000"])
        journal_file.write_text(journal_out)
        
        # Docker logs
        docker_logs = tmppath / "docker.log"
        _, docker_out, _ = run_cmd(["docker", "compose", "logs", "--tail=500"])
        docker_logs.write_text(docker_out)
        
        # Network config
        net_file = tmppath / "network.txt"
        _, net_out, _ = run_cmd(["ip", "addr"])
        _, net_route, _ = run_cmd(["ip", "route"])
        net_file.write_text(f"# Addresses\n{net_out}\n\n# Routes\n{net_route}")
        
        # ZFS status
        zfs_file = tmppath / "zfs.txt"
        _, zfs_out, _ = run_cmd(["zpool", "status", "-v"])
        zfs_file.write_text(zfs_out)
        
        # Controller config (redacted)
        config_file = tmppath / "config.json"
        try:
            import urllib.request
            req = urllib.request.Request(f"{CONTROLLER_API}/api/v1/config/export")
            with urllib.request.urlopen(req, timeout=10) as r:
                config = json.loads(r.read())
                # Redact sensitive fields
                if "mesh_ca_private_key" in config:
                    config["mesh_ca_private_key"] = "[REDACTED]"
                for key in config:
                    if "secret" in key.lower() or "password" in key.lower():
                        config[key] = "[REDACTED]"
                config_file.write_text(json.dumps(config, indent=2))
        except Exception as e:
            config_file.write_text(f"# Could not export config: {e}")
        
        # Systemd status
        systemd_file = tmppath / "systemd-status.txt"
        _, systemd_out, _ = run_cmd(["systemctl", "status", "ozma-controller", "ozma-node"])
        systemd_file.write_text(systemd_out)
        
        # Create archive
        click.echo(f"Creating support bundle: {output}")
        with gzip.open(output, "wt") as gz:
            with tarfile.open(fileobj=gz, mode="w") as tar:
                tar.add(tmppath, arcname="ozma-diagnose")
        
        click.echo(click.style(f"Support bundle created: {output}", fg="green"))
        click.echo(f"Size: {Path(output).stat().st_size / 1024:.1f} KB")


def _get_full_status() -> str:
    """Get full system status for diagnostics."""
    lines = []
    lines.append(f"Ozma Diagnose Report - {datetime.now().isoformat()}")
    lines.append("=" * 50)
    lines.append("")
    
    # Controller
    lines.append("CONTROLLER STATUS")
    lines.append("-" * 20)
    status = get_controller_status()
    lines.append(json.dumps(status, indent=2))
    lines.append("")
    
    # Systemd services
    lines.append("SYSTEMD SERVICES")
    lines.append("-" * 20)
    for svc in ["ozma-controller", "ozma-node"] + MANAGED_SERVICES:
        rc, out, _ = run_cmd(["systemctl", "is-active", svc])
        lines.append(f"{svc}: {out} (rc={rc})")
    lines.append("")
    
    # Docker
    lines.append("DOCKER SERVICES")
    lines.append("-" * 20)
    rc, out, _ = run_cmd(["docker", "ps", "--format", "{{.Names}}\t{{.Status}}"])
    lines.append(out or "No containers running")
    lines.append("")
    
    return "\n".join(lines)


# ── Entry Point ───────────────────────────────────────────────────────────────

def main():
    cli()


if __name__ == "__main__":
    main()
