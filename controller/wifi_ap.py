# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Built-in 2.4 GHz AP via hostapd — IoT SSID + onboarding SSID.

The controller can act as a Wi-Fi access point when a USB Wi-Fi dongle
(or integrated adapter) with AP mode support is available.

Two SSIDs
─────────
  IoT SSID       — permanent, default-deny isolation via nftables.
                   Intended for smart home devices after onboarding.
                   SSID name: configurable (default: "ozma-iot")
                   Key: WPA2-PSK (long random PSK, auto-generated)

  Onboarding SSID — temporary, allows the user's phone to reach the
                   IoT device's setup AP and vice versa.  Enabled
                   during onboarding sessions, auto-removed afterward.
                   SSID name: configurable (default: "ozma-setup")
                   Open (no password) — device is on an isolated subnet.

Supported hardware
──────────────────
  Any USB dongle or PCI adapter supported by nl80211 with AP capability.
  Tested with:  rtl8812au (TP-Link Archer T4U Plus)
                mt7921u  (TP-Link AXE3000 USB)
                mt76     (MT7612U based adapters)
  Requires: hostapd ≥ 2.10, iw, ip

Usage
─────
    ap = WiFiAPManager()
    await ap.start()            # starts hostapd if configured
    # IoTNetworkManager calls ap.set_onboarding_enabled(True/False)
    await ap.stop()
"""

from __future__ import annotations

import asyncio
import logging
import os
import secrets
import string
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.wifi_ap")

HOSTAPD_CONF_PATH  = Path("/tmp/ozma-hostapd.conf")
HOSTAPD_PID_PATH   = Path("/tmp/ozma-hostapd.pid")
AP_STATE_PATH      = Path(__file__).parent / "wifi_ap_state.json"

# Band and channel defaults for 2.4 GHz
DEFAULT_CHANNEL = 6
DEFAULT_HW_MODE = "g"


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclass
class WiFiAPConfig:
    """Persistent AP configuration."""
    enabled: bool = False

    # Network interface to use as the AP.
    # "auto" means probe iw list for the first device with AP capability.
    interface: str = "auto"
    resolved_interface: str = ""   # populated at start()

    # IoT SSID settings
    iot_ssid: str = "ozma-iot"
    iot_psk: str = ""              # auto-generated on first enable if empty

    # Onboarding SSID settings
    onboarding_ssid: str = "ozma-setup"
    onboarding_enabled: bool = False   # toggled per onboarding session

    # RF settings
    channel: int = DEFAULT_CHANNEL
    hw_mode: str = DEFAULT_HW_MODE
    country_code: str = "US"          # ISO 3166-1 alpha-2

    # WPA2-PSK for IoT SSID
    wpa_passphrase: str = ""   # same as iot_psk (alias for hostapd key)

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "interface": self.interface,
            "iot_ssid": self.iot_ssid,
            "iot_psk": self.iot_psk,
            "onboarding_ssid": self.onboarding_ssid,
            "onboarding_enabled": self.onboarding_enabled,
            "channel": self.channel,
            "hw_mode": self.hw_mode,
            "country_code": self.country_code,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "WiFiAPConfig":
        return cls(
            enabled=d.get("enabled", False),
            interface=d.get("interface", "auto"),
            iot_ssid=d.get("iot_ssid", "ozma-iot"),
            iot_psk=d.get("iot_psk", ""),
            onboarding_ssid=d.get("onboarding_ssid", "ozma-setup"),
            onboarding_enabled=d.get("onboarding_enabled", False),
            channel=int(d.get("channel", DEFAULT_CHANNEL)),
            hw_mode=d.get("hw_mode", DEFAULT_HW_MODE),
            country_code=d.get("country_code", "US"),
        )

    def generate_psk(self) -> str:
        """Generate a strong 24-character WPA2 passphrase."""
        alphabet = string.ascii_letters + string.digits
        return "".join(secrets.choice(alphabet) for _ in range(24))


# ---------------------------------------------------------------------------
# hostapd config generation
# ---------------------------------------------------------------------------

def _build_hostapd_conf(cfg: WiFiAPConfig, iface: str) -> str:
    """Generate a hostapd.conf with IoT SSID (and optionally onboarding SSID)."""
    lines = [
        f"interface={iface}",
        "driver=nl80211",
        f"ssid={cfg.iot_ssid}",
        f"hw_mode={cfg.hw_mode}",
        f"channel={cfg.channel}",
        f"country_code={cfg.country_code}",
        "ieee80211n=1",
        "wmm_enabled=1",
        "auth_algs=1",
        "wpa=2",
        "wpa_key_mgmt=WPA-PSK",
        "rsn_pairwise=CCMP",
        f"wpa_passphrase={cfg.iot_psk or cfg.wpa_passphrase}",
        "",
    ]

    if cfg.onboarding_enabled:
        # Add a second BSS for the onboarding SSID (open network, isolated VLAN)
        lines += [
            f"bss={iface}_0",
            f"ssid={cfg.onboarding_ssid}",
            "auth_algs=1",
            "wpa=0",
            "ignore_broadcast_ssid=0",
            "",
        ]

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Interface detection
# ---------------------------------------------------------------------------

async def _find_ap_capable_interface() -> str | None:
    """Return the first wireless interface with AP capability, or None."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "iw", "list",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        text = out.decode(errors="replace")
    except FileNotFoundError:
        log.warning("'iw' not found — cannot auto-detect Wi-Fi AP interface")
        return None

    iface: str | None = None
    in_ap_section = False
    for line in text.splitlines():
        if "Wiphy " in line:
            iface = line.split()[-1]
            in_ap_section = False
        if "Supported interface modes:" in line:
            in_ap_section = True
        if in_ap_section and "* AP" in line:
            # Map wiphy name to actual interface
            mapped = await _wiphy_to_interface(iface or "")
            if mapped:
                return mapped
    return None


async def _wiphy_to_interface(wiphy: str) -> str | None:
    """Resolve a wiphy name (e.g. phy0) to a netdev interface name."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "iw", "dev",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        text = out.decode(errors="replace")
    except FileNotFoundError:
        return None

    current_phy = ""
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("phy#"):
            current_phy = line.replace("phy#", "phy")
        elif line.startswith("Interface ") and current_phy == wiphy:
            return line.split()[-1]
    return None


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class WiFiAPManager:
    """Manages a hostapd-backed dual-SSID Wi-Fi AP."""

    def __init__(self, state_path: Path = AP_STATE_PATH) -> None:
        self._state_path = state_path
        self._config = WiFiAPConfig()
        self._proc: asyncio.subprocess.Process | None = None
        self._running = False
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._load()
        if self._config.enabled:
            await self._start_hostapd()
        log.info("WiFiAPManager started (enabled=%s)", self._config.enabled)

    async def stop(self) -> None:
        await self._stop_hostapd()
        log.info("WiFiAPManager stopped")

    async def _start_hostapd(self) -> None:
        iface = self._config.interface
        if iface == "auto":
            iface = await _find_ap_capable_interface() or ""
            if not iface:
                log.warning("No AP-capable Wi-Fi interface found — AP not started")
                return
            self._config.resolved_interface = iface

        if not self._config.iot_psk:
            self._config.iot_psk = self._config.generate_psk()
            self._save()

        conf = _build_hostapd_conf(self._config, iface)
        HOSTAPD_CONF_PATH.write_text(conf)
        HOSTAPD_CONF_PATH.chmod(0o600)

        try:
            self._proc = await asyncio.create_subprocess_exec(
                "hostapd", "-P", str(HOSTAPD_PID_PATH), str(HOSTAPD_CONF_PATH),
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._running = True
            log.info("hostapd started on %s (SSIDs: %s, %s)",
                     iface, self._config.iot_ssid,
                     self._config.onboarding_ssid if self._config.onboarding_enabled else "-")
        except FileNotFoundError:
            log.warning("hostapd not installed — built-in AP not available (install hostapd)")
        except Exception as exc:
            log.error("hostapd failed to start: %s", exc)

    async def _stop_hostapd(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._proc = None
        self._running = False
        HOSTAPD_CONF_PATH.unlink(missing_ok=True)

    async def _restart_hostapd(self) -> None:
        await self._stop_hostapd()
        if self._config.enabled:
            await self._start_hostapd()

    # ------------------------------------------------------------------
    # Configuration
    # ------------------------------------------------------------------

    def get_config(self) -> WiFiAPConfig:
        return self._config

    async def set_config(self, **updates) -> WiFiAPConfig:
        restart_needed = False
        for key, value in updates.items():
            if hasattr(self._config, key):
                if getattr(self._config, key) != value:
                    setattr(self._config, key, value)
                    restart_needed = True
        self._save()
        if restart_needed:
            await self._restart_hostapd()
        return self._config

    def get_status(self) -> dict:
        return {
            "enabled": self._config.enabled,
            "running": self._running,
            "interface": self._config.resolved_interface or self._config.interface,
            "iot_ssid": self._config.iot_ssid,
            "onboarding_ssid": self._config.onboarding_ssid if self._config.onboarding_enabled else None,
            "channel": self._config.channel,
            "country_code": self._config.country_code,
        }

    # ------------------------------------------------------------------
    # Onboarding SSID toggle (called by IoTNetworkManager)
    # ------------------------------------------------------------------

    async def set_onboarding_enabled(self, enabled: bool) -> None:
        """Enable or disable the onboarding (open) SSID.  Restarts hostapd."""
        if self._config.onboarding_enabled == enabled:
            return
        self._config.onboarding_enabled = enabled
        self._save()
        if self._running:
            await self._restart_hostapd()

    # ------------------------------------------------------------------
    # Probe available interfaces
    # ------------------------------------------------------------------

    async def probe_interfaces(self) -> list[dict]:
        """Return a list of Wi-Fi interfaces with their AP capability status."""
        result = []
        try:
            proc = await asyncio.create_subprocess_exec(
                "iw", "dev",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            text = out.decode(errors="replace")
        except FileNotFoundError:
            return result

        iface = ""
        for line in text.splitlines():
            line = line.strip()
            if line.startswith("Interface "):
                iface = line.split()[-1]
                result.append({"interface": iface, "ap_capable": False})
            elif "type" in line and iface:
                for entry in result:
                    if entry["interface"] == iface:
                        entry["current_mode"] = line.split()[-1]

        return result

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        import json
        tmp = self._state_path.with_suffix(".tmp")
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(self._config.to_dict(), indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        import json
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            self._config = WiFiAPConfig.from_dict(data)
        except Exception:
            log.exception("Failed to load WiFi AP state")
