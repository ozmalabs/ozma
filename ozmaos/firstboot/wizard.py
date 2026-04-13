#!/usr/bin/env python3
"""
OzmaOS First-Boot Wizard

Runs on first login after installing OzmaOS. Uses whiptail for TUI.
Called by systemd service ozmaos-firstboot.service.
"""

import os
import sys
import subprocess
import json
import re
from pathlib import Path
from typing import Optional

import detect_hardware
import apply_config
import services


class Whiptail:
    """Wrapper for whiptail dialogs."""
    
    def __init__(self, title: str = "OzmaOS"):
        self.title = title
        self.default_item = None
    
    def msgbox(self, message: str, height: int = 20, width: int = 60) -> bool:
        return self._run("msgbox", message, height, width)
    
    def yesno(self, message: str, height: int = 15, width: int = 60) -> bool:
        result = self._run("yesno", message, height, width)
        return result == 0
    
    def inputbox(self, message: str, default: str = "", height: int = 12, width: int = 60) -> tuple[bool, str]:
        """Returns (ok_pressed, value)."""
        height = max(12, len(message.split('\n')) + 4)
        proc = subprocess.run(
            ["whiptail", "--title", self.title, "--inputbox", message, str(height), str(width), default],
            capture_output=True, text=True
        )
        return proc.returncode == 0, proc.stdout.strip()
    
    def passwordbox(self, message: str, height: int = 12, width: int = 60) -> tuple[bool, str]:
        """Returns (ok_pressed, value)."""
        height = max(12, len(message.split('\n')) + 4)
        proc = subprocess.run(
            ["whiptail", "--title", self.title, "--passwordbox", message, str(height), str(width)],
            capture_output=True, text=True
        )
        return proc.returncode == 0, proc.stdout.strip()
    
    def menu(self, message: str, items: list[tuple[str, str]], height: int = 20, width: int = 60) -> tuple[bool, str]:
        """items: list of (tag, description)."""
        args = ["whiptail", "--title", self.title, "--menu", message, str(height), str(width), str(len(items))]
        for tag, desc in items:
            args.append(tag)
            args.append(desc)
        proc = subprocess.run(args, capture_output=True, text=True)
        return proc.returncode == 0, proc.stdout.strip()
    
    def radiolist(self, message: str, items: list[tuple[str, str, bool]], height: int = 20, width: int = 60) -> tuple[bool, str]:
        """items: list of (tag, description, selected)."""
        args = ["whiptail", "--title", self.title, "--radiolist", message, str(height), str(width), str(len(items))]
        for tag, desc, selected in items:
            args.append(tag)
            args.append(desc)
            args.append("ON" if selected else "OFF")
        proc = subprocess.run(args, capture_output=True, text=True)
        return proc.returncode == 0, proc.stdout.strip()
    
    def checklist(self, message: str, items: list[tuple[str, str, bool]], height: int = 20, width: int = 60) -> tuple[bool, list[str]]:
        """items: list of (tag, description, selected). Returns selected tags."""
        args = ["whiptail", "--title", self.title, "--checklist", message, str(height), str(width), str(len(items))]
        for tag, desc, selected in items:
            args.append(tag)
            args.append(desc)
            args.append("ON" if selected else "OFF")
        proc = subprocess.run(args, capture_output=True, text=True)
        if proc.returncode != 0:
            return False, []
        selected = [s.strip('"') for s in proc.stdout.split()]
        return True, selected
    
    def gauge(self, message: str, height: int = 12, width: int = 60, percent: int = 0) -> None:
        proc = subprocess.Popen(
            ["whiptail", "--title", self.title, "--gauge", message, str(height), str(width), str(percent)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        # Send EOF to close gauge
        proc.communicate()
    
    def set_progress(self, percent: int) -> None:
        """Update gauge progress via stdin."""
        if hasattr(self, '_gauge_proc') and self._gauge_proc.poll() is None:
            self._gauge_proc.stdin.write(f"{percent}\n")
            self._gauge_proc.stdin.flush()
    
    def _run(self, dialog: str, message: str, height: int, width: int, *extra) -> int:
        args = ["whiptail", "--title", self.title, f"--{dialog}", message, str(height), str(width)] + list(extra)
        return subprocess.run(args).returncode


def get_mac_address() -> str:
    """Get last 4 characters of MAC address from eth0."""
    try:
        with open("/sys/class/net/eth0/address", "r") as f:
            mac = f.read().strip()
            return mac.replace(":", "")[-4:]
    except:
        return "0000"


def get_default_hostname() -> str:
    """Generate default hostname: ozma-<last4-mac>."""
    mac_suffix = get_mac_address()
    return f"ozma-{mac_suffix}"


def get_common_timezones() -> list[tuple[str, str]]:
    """Get list of common timezones."""
    timezones = [
        ("America/New_York", "Eastern Time (US & Canada)"),
        ("America/Chicago", "Central Time (US & Canada)"),
        ("America/Denver", "Mountain Time (US & Canada)"),
        ("America/Los_Angeles", "Pacific Time (US & Canada)"),
        ("America/Toronto", "Toronto"),
        ("America/Vancouver", "Vancouver"),
        ("America/Sao_Paulo", "Sao Paulo"),
        ("Europe/London", "London"),
        ("Europe/Paris", "Paris"),
        ("Europe/Berlin", "Berlin"),
        ("Europe/Amsterdam", "Amsterdam"),
        ("Europe/Stockholm", "Stockholm"),
        ("Europe/Moscow", "Moscow"),
        ("Asia/Dubai", "Dubai"),
        ("Asia/Kolkata", "Mumbai, New Delhi"),
        ("Asia/Bangkok", "Bangkok"),
        ("Asia/Singapore", "Singapore"),
        ("Asia/Hong_Kong", "Hong Kong"),
        ("Asia/Tokyo", "Tokyo"),
        ("Asia/Seoul", "Seoul"),
        ("Australia/Sydney", "Sydney"),
        ("Australia/Melbourne", "Melbourne"),
        ("Pacific/Auckland", "Auckland"),
    ]
    return timezones


def get_disk_info() -> list[tuple[str, str, str]]:
    """Get list of disks with sizes. Returns list of (device, size, model)."""
    disks = []
    try:
        result = subprocess.run(
            ["lsblk", "-d", "-n", "-o", "NAME,SIZE,TYPE", "--json"],
            capture_output=True, text=True, timeout=10
        )
        data = json.loads(result.stdout)
        for device in data.get("blockdevices", []):
            if device.get("type") == "disk" and device.get("name", "").startswith("sd"):
                name = f"/dev/{device['name']}"
                size = device.get("size", "unknown")
                model = device.get("model", "Unknown")
                disks.append((name, size, model))
    except Exception as e:
        print(f"Error getting disk info: {e}", file=sys.stderr)
    return disks


def validate_ip(ip: str) -> bool:
    """Validate IPv4 address."""
    pattern = r'^(\d{1,3}\.){3}\d{1,3}$'
    if not re.match(pattern, ip):
        return False
    parts = ip.split('.')
    return all(0 <= int(p) <= 255 for p in parts)


def validate_hostname(hostname: str) -> bool:
    """Validate hostname."""
    if not hostname or len(hostname) > 63:
        return False
    pattern = r'^[a-zA-Z0-9]([a-zA-Z0-9\-]{0,61}[a-zA-Z0-9])?$'
    return bool(re.match(pattern, hostname))


def validate_username(username: str) -> bool:
    """Validate Linux username."""
    pattern = r'^[a-z_][a-z0-9_-]*[$]?$'
    return bool(re.match(pattern, username)) and len(username) <= 32


class Wizard:
    def __init__(self):
        self.ui = Whiptail("OzmaOS First-Boot Wizard")
        self.config = {}
        self.hardware = detect_hardware.detect()
        self.steps = [
            ("welcome", self.step_welcome),
            ("hostname", self.step_hostname),
            ("network", self.step_network),
            ("timezone", self.step_timezone),
            ("admin_user", self.step_admin_user),
            ("services", self.step_services),
            ("zfs", self.step_zfs),
            ("connect", self.step_connect),
            ("summary", self.step_summary),
            ("apply", self.step_apply),
            ("done", self.step_done),
        ]
    
    def run(self):
        """Run the wizard."""
        for step_name, step_func in self.steps:
            if not step_func():
                # User cancelled
                self.ui.msgbox("Wizard cancelled. Run 'ozmaos-firstboot' to restart.")
                sys.exit(1)
    
    def step_welcome(self) -> bool:
        """Welcome screen."""
        msg = "Welcome to OzmaOS!\n\n"
        msg += "This wizard will help you configure your system.\n"
        msg += "You can safely skip optional steps.\n\n"
        msg += "Press Enter to continue."
        return self.ui.msgbox(msg, height=15)
    
    def step_hostname(self) -> bool:
        """Set system hostname."""
        default = get_default_hostname()
        ok, hostname = self.ui.inputbox(
            "Enter the hostname for this system.\n\n"
            "This will be used to identify this machine on your network.\n"
            "Example: ozma-abc1",
            default=default
        )
        if not ok:
            return False
        
        hostname = hostname.strip()
        if not hostname:
            self.ui.msgbox("Error: Hostname cannot be empty.")
            return True  # Retry
        
        if not validate_hostname(hostname):
            self.ui.msgbox("Error: Invalid hostname. Use letters, numbers, and hyphens only.")
            return True  # Retry
        
        self.config["hostname"] = hostname
        return True
    
    def step_network(self) -> bool:
        """Configure network."""
        ok, mode = self.ui.radiolist(
            "Select network configuration:\n\n"
            "DHCP: Automatically obtain IP address (recommended)\n"
            "Static: Manually configure IP address",
            [
                ("dhcp", "DHCP (automatic)", True),
                ("static", "Static IP", False),
            ],
            height=15
        )
        if not ok:
            return False
        
        self.config["network"] = {"mode": mode}
        
        if mode == "static":
            # Get current IP for suggestion
            ok_ip, ip = self.ui.inputbox("Enter IP address (e.g., 192.168.1.100):", default="192.168.1.100")
            if not ok_ip:
                return False
            
            ok_gw, gateway = self.ui.inputbox("Enter gateway (router) IP:", default="192.168.1.1")
            if not ok_gw:
                return False
            
            ok_dns, dns = self.ui.inputbox("Enter DNS server IP:", default="1.1.1.1")
            if not ok_dns:
                return False
            
            # Validate
            if not validate_ip(ip) or not validate_ip(gateway) or not validate_ip(dns):
                self.ui.msgbox("Error: Invalid IP address format.")
                return True
            
            self.config["network"]["static"] = {
                "ip": ip,
                "gateway": gateway,
                "dns": dns,
            }
        
        return True
    
    def step_timezone(self) -> bool:
        """Set timezone."""
        timezones = get_common_timezones()
        
        items = [(tz, desc, tz == "America/New_York") for tz, desc in timezones]
        
        ok, timezone = self.ui.menu(
            "Select your timezone:",
            items,
            height=20
        )
        if not ok:
            return False
        
        self.config["timezone"] = timezone
        return True
    
    def step_admin_user(self) -> bool:
        """Create admin user."""
        ok, username = self.ui.inputbox(
            "Enter the admin username:\n\n"
            "This user will have sudo privileges.",
            default="admin"
        )
        if not ok:
            return False
        
        username = username.strip().lower()
        if not validate_username(username):
            self.ui.msgbox("Error: Invalid username. Use lowercase letters, numbers, underscores, and hyphens.")
            return True
        
        ok, password = self.ui.passwordbox("Enter password:")
        if not ok:
            return False
        
        if len(password) < 8:
            self.ui.msgbox("Error: Password must be at least 8 characters.")
            return True
        
        ok, password_confirm = self.ui.passwordbox("Confirm password:")
        if not ok:
            return False
        
        if password != password_confirm:
            self.ui.msgbox("Error: Passwords do not match.")
            return True
        
        self.config["admin_user"] = {
            "username": username,
            "password": password,
        }
        return True
    
    def step_services(self) -> bool:
        """Select services to enable."""
        service_list = [
            ("controller", "Ozma Controller", True),
            ("frigate", "Frigate (camera NVR)", True),
            ("vaultwarden", "Vaultwarden (password manager)", True),
            ("jellyfin", "Jellyfin (media server)", False),
            ("immich", "Immich (photo backup)", False),
            ("authentik", "Authentik (identity provider)", False),
            ("ollama", "Ollama (local AI)", False),
        ]
        
        # Filter out unavailable services
        available = services.get_available_services(self.hardware)
        service_list = [(k, name, default) for k, name, default in service_list if k in available]
        
        ok, selected = self.ui.checklist(
            "Select services to enable:\n\n"
            "Press Space to toggle, Enter to confirm.\n\n"
            "Services marked [*] are recommended for most users.",
            service_list,
            height=20
        )
        if not ok:
            return False
        
        self.config["services"] = selected
        return True
    
    def step_zfs(self) -> bool:
        """ZFS pool configuration (optional)."""
        disks = self.hardware.get("disks", [])
        extra_disks = [d for d in disks if d.get("is_extra", False)]
        
        if len(extra_disks) < 2:
            # Not enough disks for ZFS
            self.config["zfs"] = {"enabled": False}
            return True
        
        # Show disk list
        disk_desc = "\n".join([f"  {d['device']} ({d['size']}) - {d.get('model', 'Unknown')}" for d in extra_disks])
        
        items = [
            ("skip", "Skip (no additional storage pool)", True),
            ("mirror", "Mirror (2 disks, redundancy)", False),
            ("raidz1", "RAIDZ1 (3+ disks, better capacity)", False),
        ]
        
        ok, mode = self.ui.radiolist(
            f"ZFS Pool Configuration\n\n"
            f"Detected {len(extra_disks)} extra disk(s):\n{disk_desc}\n\n"
            f"Select pool configuration:",
            items,
            height=20
        )
        if not ok:
            return False
        
        if mode == "skip":
            self.config["zfs"] = {"enabled": False}
        else:
            self.config["zfs"] = {
                "enabled": True,
                "mode": mode,
                "disks": [d["device"] for d in extra_disks[:4]],  # Limit to 4 disks for sanity
            }
        
        return True
    
    def step_connect(self) -> bool:
        """Ozma Connect configuration."""
        ok = self.ui.yesno(
            "Connect to Ozma Connect?\n\n"
            "Ozma Connect provides:\n"
            "  • Remote access to your desktop\n"
            "  • Automatic backups\n"
            "  • Easy sharing with guests\n\n"
            "Sign up at https://connect.ozmaos.com",
            height=15
        )
        
        if not ok:
            # User chose "No"
            self.config["connect"] = {"enabled": False}
            return True
        
        ok, email = self.ui.inputbox("Enter your Ozma Connect email:")
        if not ok:
            return False
        
        ok, password = self.ui.passwordbox("Enter your Ozma Connect password:")
        if not ok:
            return False
        
        self.config["connect"] = {
            "enabled": True,
            "email": email,
            "password": password,
        }
        return True
    
    def step_summary(self) -> bool:
        """Show summary of all choices."""
        lines = ["Configuration Summary", ""]
        
        lines.append(f"Hostname: {self.config.get('hostname', 'N/A')}")
        
        net = self.config.get("network", {})
        if net.get("mode") == "dhcp":
            lines.append("Network: DHCP (automatic)")
        else:
            lines.append(f"Network: Static ({net.get('static', {}).get('ip', 'N/A')})")
        
        lines.append(f"Timezone: {self.config.get('timezone', 'N/A')}")
        
        user = self.config.get("admin_user", {})
        lines.append(f"Admin user: {user.get('username', 'N/A')}")
        
        services_list = self.config.get("services", [])
        lines.append(f"Services: {', '.join(services_list) if services_list else 'None'}")
        
        zfs = self.config.get("zfs", {})
        if zfs.get("enabled"):
            lines.append(f"ZFS Pool: {zfs.get('mode', 'unknown').upper()} ({', '.join(zfs.get('disks', []))})")
        else:
            lines.append("ZFS Pool: Disabled")
        
        connect = self.config.get("connect", {})
        lines.append(f"Ozma Connect: {'Enabled' if connect.get('enabled') else 'Disabled'}")
        
        msg = "\n".join(lines)
        
        return self.ui.yesno(msg + "\n\nApply these settings?", height=25)
    
    def step_apply(self) -> bool:
        """Apply all configuration."""
        # Create progress gauge
        steps = [
            ("Applying network configuration...", 10),
            ("Setting hostname...", 20),
            ("Setting timezone...", 30),
            ("Creating admin user...", 40),
            ("Enabling services...", 60),
            ("Creating ZFS pool...", 80),
            ("Connecting to Ozma Connect...", 90),
            ("Finalizing configuration...", 100),
        ]
        
        # Run apply_config with progress callback
        def progress_callback(step: int, message: str):
            if step < len(steps):
                self.ui.gauge(message, percent=steps[step][1])
        
        try:
            # Show initial gauge
            self.ui.gauge("Starting configuration...", percent=0)
            
            # Apply each step
            apply_config.apply(self.config, self.hardware)
            
            # Mark firstboot complete
            Path("/etc/ozmaos/firstboot-complete").touch()
            
            return True
        except Exception as e:
            self.ui.msgbox(f"Error applying configuration:\n\n{str(e)}\n\nPlease check the logs and run 'ozmaos-firstboot' to retry.")
            return False
    
    def step_done(self) -> bool:
        """Show completion message."""
        hostname = self.config.get("hostname", "localhost")
        
        msg = (
            "Configuration complete!\n\n"
            f"Access the Ozma Controller at:\n\n"
            f"  http://{hostname}:7380\n\n"
            "For the web interface, open this URL in your browser.\n\n"
            "Thank you for choosing OzmaOS!"
        )
        
        return self.ui.msgbox(msg, height=15)


def main():
    """Main entry point."""
    # Check if already completed
    if Path("/etc/ozmaos/firstboot-complete").exists():
        print("First boot wizard already completed.")
        print("Run 'sudo rm /etc/ozmaos/firstboot-complete' to re-run.")
        sys.exit(0)
    
    # Check if running as root
    if os.geteuid() != 0:
        print("This script must be run as root.")
        sys.exit(1)
    
    # Run wizard
    wizard = Wizard()
    wizard.run()


if __name__ == "__main__":
    main()
