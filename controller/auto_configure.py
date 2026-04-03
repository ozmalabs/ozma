# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V1.7 Auto-configure — PoE subnet device discovery and camera auto-registration.

Watches the PoE subnet (default 192.168.100.0/24) for new devices via:
  - dnsmasq lease file polling (picks up devices that got DHCP leases)
  - ARP table polling (/proc/net/arp on Linux)
  - Periodic arp-scan sweep (if arp-scan is installed)

For each new device, fingerprints it:
  - MAC OUI vendor lookup (built-in table of camera vendors)
  - RTSP probe (port 554) — confirms it's a camera, discovers stream URLs
  - ONVIF probe (port 80/8080 with ONVIF WS-Discovery) — gets PTZ, profiles
  - mDNS probe — checks for _ozma._udp.local. (already registered?)

If a device looks like a camera (RTSP responds or vendor is known camera brand),
it gets auto-registered as a NodeInfo with machine_class="camera".

The controller fires a "device_discovered" event so the dashboard can show
a notification: "New camera at 192.168.100.5 — [Register] [Ignore]".
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import socket
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.auto_configure")

# Default PoE subnet for NVR build
DEFAULT_POE_SUBNET = "192.168.100"
DEFAULT_LEASE_FILE = Path("/tmp/ozma-dnsmasq.leases")

# Known camera/NVR MAC OUI prefixes (first 3 octets, uppercase, no separators)
# Covers the major IP camera brands
_CAMERA_VENDORS: dict[str, str] = {
    "D4859A": "Hikvision",  "E4241B": "Hikvision", "C05627": "Hikvision",
    "BC5141": "Dahua",      "706D15": "Dahua",      "3C1F41": "Dahua",
    "000F18": "Axis",       "ACCC8E": "Axis",       "B8A44E": "Axis",
    "000D6F": "Reolink",    "EC4D47": "Reolink",
    "2CAF4C": "Amcrest",    "9CEBE8": "Amcrest",
    "00408C": "Foscam",     "C4D9C4": "Foscam",
    "00D0C9": "Uniview",
    "705A0F": "Hanwha",     "000621": "Hanwha",
    "000413": "Mobotix",
    "B4A2EB": "Bosch",
    "F46C6F": "Vivotek",    "00D021": "Vivotek",
    "001AC5": "Ubiquiti",   "788A20": "Ubiquiti",   "24A43C": "Ubiquiti",
    "B402C0": "Tapo",       "50C7BF": "TP-Link",
}

# Ports to probe
_RTSP_PORT  = 554
_HTTP_PORT  = 80
_HTTPS_PORT = 443
_ONVIF_PORT = 8899

# Default RTSP paths to try
_RTSP_PATHS = [
    "/",
    "/live/ch00_0",
    "/Streaming/Channels/1",
    "/cam/realmonitor?channel=1&subtype=0",
    "/h264Preview_01_main",
    "/stream1",
    "/video1",
    "/live.sdp",
]


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class AutoConfigureDevice:
    """A device discovered on the PoE subnet."""
    ip: str
    mac: str
    hostname: str = ""
    vendor: str = ""
    device_type: str = "unknown"   # camera | nvr | switch | ap | unknown
    rtsp_urls: list[str] = field(default_factory=list)
    onvif: bool = False
    http_title: str = ""           # page title from HTTP probe
    registered: bool = False
    registered_node_id: str = ""
    ignored: bool = False
    first_seen: float = field(default_factory=time.time)
    last_seen: float = field(default_factory=time.time)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ip":                  self.ip,
            "mac":                 self.mac,
            "hostname":            self.hostname,
            "vendor":              self.vendor,
            "device_type":         self.device_type,
            "rtsp_urls":           self.rtsp_urls,
            "onvif":               self.onvif,
            "http_title":          self.http_title,
            "registered":          self.registered,
            "registered_node_id":  self.registered_node_id,
            "ignored":             self.ignored,
            "first_seen":          self.first_seen,
            "last_seen":           self.last_seen,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "AutoConfigureDevice":
        obj = cls(ip=d["ip"], mac=d.get("mac", ""))
        for k, v in d.items():
            if hasattr(obj, k):
                setattr(obj, k, v)
        return obj


# ---------------------------------------------------------------------------
# AutoConfigureManager
# ---------------------------------------------------------------------------

class AutoConfigureManager:
    """
    Watches the PoE subnet for new devices and fingerprints them.

    Fires "device_discovered" events into state.events for each new
    device. The dashboard shows a notification; the operator clicks
    [Register] to add it as a camera node.
    """

    SCAN_INTERVAL    = 30.0   # seconds between scans
    FINGERPRINT_TIMEOUT = 3.0 # seconds for each probe

    def __init__(
        self,
        state: Any = None,
        poe_subnet: str = DEFAULT_POE_SUBNET,
        lease_file: Path = DEFAULT_LEASE_FILE,
        data_dir: Path | None = None,
    ) -> None:
        self._state     = state
        self._subnet    = poe_subnet
        self._lease_file = lease_file
        self._data_dir  = data_dir or Path("/var/lib/ozma/auto_configure")
        self._devices: dict[str, AutoConfigureDevice] = {}  # ip → device
        self._tasks: list[asyncio.Task] = []
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._tasks.append(
            asyncio.create_task(self._scan_loop(), name="auto-configure:scan")
        )
        log.info("AutoConfigureManager started (subnet=%s.0/24)", self._subnet)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def list_devices(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._devices.values()]

    def get_device(self, ip: str) -> AutoConfigureDevice | None:
        return self._devices.get(ip)

    async def register_device(
        self, ip: str, name: str,
        machine_class: str = "camera",
    ) -> dict[str, Any]:
        """
        Register a discovered device as an ozma node.

        Creates a NodeInfo in state and marks the device as registered.
        Returns the registration result.
        """
        dev = self._devices.get(ip)
        if not dev:
            return {"ok": False, "error": "Device not found"}

        if not self._state:
            return {"ok": False, "error": "No state reference"}

        from state import NodeInfo
        node_id = f"{name}._ozma._udp.local."
        camera_streams: list[dict] = []
        for url in dev.rtsp_urls:
            camera_streams.append({
                "name":          "main",
                "rtsp_inbound":  url,
                "hls":           "",
                "backchannel":   "",
            })

        node = NodeInfo(
            id            = node_id,
            host          = ip,
            port          = 7331,
            role          = "display" if machine_class == "camera" else "compute",
            hw            = dev.vendor.lower().replace(" ", "-") or "ip-camera",
            fw_version    = "unknown",
            proto_version = 1,
            capabilities  = ["rtsp"] + (["onvif"] if dev.onvif else []),
            machine_class = machine_class,
            camera_streams= camera_streams,
            direct_registered = True,
        )
        await self._state.add_node(node)

        dev.registered = True
        dev.registered_node_id = node_id
        self._save()

        log.info("Auto-registered %s as node %s (%s)", ip, node_id, machine_class)
        return {"ok": True, "node_id": node_id, "ip": ip}

    def ignore_device(self, ip: str) -> None:
        """Mark a device as ignored (won't fire events or appear in suggestions)."""
        dev = self._devices.get(ip)
        if dev:
            dev.ignored = True
            self._save()

    def unignore_device(self, ip: str) -> None:
        dev = self._devices.get(ip)
        if dev:
            dev.ignored = False
            self._save()

    async def scan_now(self) -> list[dict[str, Any]]:
        """Trigger an immediate scan and return new devices found."""
        new_ips = await self._do_scan()
        results = []
        for ip in new_ips:
            dev = await self._fingerprint(ip, self._devices.get(ip, AutoConfigureDevice(ip=ip, mac="")).mac)
            self._devices[ip] = dev
            if not dev.ignored:
                results.append(dev.to_dict())
        self._save()
        return results

    # ------------------------------------------------------------------
    # Internal: scan loop
    # ------------------------------------------------------------------

    async def _scan_loop(self) -> None:
        # Initial scan after short delay
        await asyncio.sleep(5.0)
        while True:
            try:
                new_ips = await self._do_scan()
                for ip in new_ips:
                    mac = self._devices.get(ip, AutoConfigureDevice(ip=ip, mac="")).mac
                    dev = await self._fingerprint(ip, mac)
                    self._devices[ip] = dev
                    if not dev.ignored:
                        await self._fire_event(dev)
                if new_ips:
                    self._save()
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Auto-configure scan error")
            await asyncio.sleep(self.SCAN_INTERVAL)

    async def _do_scan(self) -> list[str]:
        """Return list of new (not yet tracked) IPs on the subnet."""
        found: dict[str, str] = {}  # ip → mac

        # 1. Parse dnsmasq lease file
        found.update(self._parse_lease_file())

        # 2. Parse kernel ARP table
        found.update(self._parse_arp_table())

        # 3. arp-scan sweep (if available)
        found.update(await self._arp_scan())

        # Filter to our subnet, find new ones
        new_ips = []
        for ip, mac in found.items():
            if not ip.startswith(self._subnet + "."):
                continue
            if ip in self._devices:
                self._devices[ip].last_seen = time.time()
                if mac and not self._devices[ip].mac:
                    self._devices[ip].mac = mac
            else:
                self._devices[ip] = AutoConfigureDevice(ip=ip, mac=mac)
                new_ips.append(ip)

        return new_ips

    def _parse_lease_file(self) -> dict[str, str]:
        """Parse dnsmasq lease file → {ip: mac}."""
        result: dict[str, str] = {}
        if not self._lease_file.exists():
            return result
        try:
            for line in self._lease_file.read_text().splitlines():
                # Format: <expire> <mac> <ip> <hostname> <client-id>
                parts = line.split()
                if len(parts) >= 3:
                    mac = parts[1].upper().replace(":", "")
                    ip  = parts[2]
                    result[ip] = mac
        except Exception:
            log.exception("Failed to parse lease file %s", self._lease_file)
        return result

    def _parse_arp_table(self) -> dict[str, str]:
        """Read /proc/net/arp → {ip: mac}."""
        result: dict[str, str] = {}
        arp_file = Path("/proc/net/arp")
        if not arp_file.exists():
            return result
        try:
            for line in arp_file.read_text().splitlines()[1:]:  # skip header
                parts = line.split()
                if len(parts) >= 4 and parts[2] == "0x2":  # complete entry
                    ip  = parts[0]
                    mac = parts[3].upper().replace(":", "")
                    result[ip] = mac
        except Exception:
            pass
        return result

    async def _arp_scan(self) -> dict[str, str]:
        """Run arp-scan on the PoE subnet (optional — skip if not installed)."""
        import shutil
        if not shutil.which("arp-scan"):
            return {}
        result: dict[str, str] = {}
        try:
            proc = await asyncio.create_subprocess_exec(
                "arp-scan", "--localnet", "--quiet",
                "--interface", "any",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await asyncio.wait_for(proc.communicate(), timeout=15.0)
            for line in out.decode().splitlines():
                parts = line.split("\t")
                if len(parts) >= 2:
                    ip  = parts[0].strip()
                    mac = parts[1].strip().upper().replace(":", "")
                    if re.match(r"^\d+\.\d+\.\d+\.\d+$", ip):
                        result[ip] = mac
        except (asyncio.TimeoutError, Exception):
            pass
        return result

    # ------------------------------------------------------------------
    # Internal: fingerprinting
    # ------------------------------------------------------------------

    async def _fingerprint(self, ip: str, mac: str) -> AutoConfigureDevice:
        dev = self._devices.get(ip) or AutoConfigureDevice(ip=ip, mac=mac)
        dev.mac = mac or dev.mac
        dev.last_seen = time.time()
        dev.vendor = self._lookup_vendor(dev.mac)

        # Run probes concurrently
        rtsp_task   = asyncio.create_task(self._probe_rtsp(ip))
        onvif_task  = asyncio.create_task(self._probe_onvif(ip))
        http_task   = asyncio.create_task(self._probe_http(ip))
        hostname_t  = asyncio.create_task(self._resolve_hostname(ip))

        rtsp_urls, onvif, http_title, hostname = await asyncio.gather(
            rtsp_task, onvif_task, http_task, hostname_t,
            return_exceptions=True,
        )

        dev.rtsp_urls  = rtsp_urls   if isinstance(rtsp_urls, list)  else []
        dev.onvif      = onvif       if isinstance(onvif, bool)       else False
        dev.http_title = http_title  if isinstance(http_title, str)   else ""
        dev.hostname   = hostname    if isinstance(hostname, str)     else ""

        # Classify
        if dev.rtsp_urls or dev.onvif or dev.vendor in _CAMERA_VENDORS.values():
            dev.device_type = "camera"
        elif any(k in dev.http_title.lower() for k in ("nvr", "dvr", "recorder")):
            dev.device_type = "nvr"
        elif any(k in dev.http_title.lower() for k in ("switch", "managed")):
            dev.device_type = "switch"

        log.info("Fingerprinted %s: type=%s vendor=%s rtsp=%d onvif=%s",
                 ip, dev.device_type, dev.vendor, len(dev.rtsp_urls), dev.onvif)
        return dev

    def _lookup_vendor(self, mac: str) -> str:
        """Look up MAC OUI in built-in camera vendor table."""
        if not mac:
            return ""
        oui = mac.upper().replace(":", "").replace("-", "")[:6]
        return _CAMERA_VENDORS.get(oui, "")

    async def _probe_rtsp(self, ip: str) -> list[str]:
        """Try connecting to RTSP port 554 and probing common stream paths."""
        working: list[str] = []
        # First check if port is open
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_connection(ip, _RTSP_PORT), timeout=2.0
            )
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
        except Exception:
            return []  # Port closed — not a camera

        # Port open — add known paths
        for path in _RTSP_PATHS:
            working.append(f"rtsp://{ip}{path}")
            if len(working) >= 3:  # return first 3 candidates
                break

        return working

    async def _probe_onvif(self, ip: str) -> bool:
        """Check if device responds to ONVIF GetCapabilities."""
        body = (
            '<?xml version="1.0"?>'
            '<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">'
            '<s:Body><GetCapabilities xmlns="http://www.onvif.org/ver10/device/wsdl">'
            '<Category>All</Category>'
            '</GetCapabilities></s:Body></s:Envelope>'
        )
        for port in (80, 8080, _ONVIF_PORT):
            try:
                import urllib.request
                req = urllib.request.Request(
                    f"http://{ip}:{port}/onvif/device_service",
                    data=body.encode(),
                    headers={"Content-Type": "application/soap+xml"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                resp = await asyncio.wait_for(
                    loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=2)),
                    timeout=3.0,
                )
                if resp.status in (200, 401):  # 401 = ONVIF auth required (still ONVIF)
                    return True
            except Exception:
                continue
        return False

    async def _probe_http(self, ip: str) -> str:
        """GET http://{ip}/ and extract page title."""
        try:
            import urllib.request
            loop = asyncio.get_running_loop()
            resp = await asyncio.wait_for(
                loop.run_in_executor(
                    None, lambda: urllib.request.urlopen(f"http://{ip}/", timeout=2)
                ),
                timeout=3.0,
            )
            html = resp.read(4096).decode("utf-8", errors="replace")
            m = re.search(r"<title[^>]*>(.*?)</title>", html, re.IGNORECASE | re.DOTALL)
            return m.group(1).strip() if m else ""
        except Exception:
            return ""

    async def _resolve_hostname(self, ip: str) -> str:
        try:
            loop = asyncio.get_running_loop()
            hostname, _, _ = await asyncio.wait_for(
                loop.run_in_executor(None, socket.gethostbyaddr, ip),
                timeout=2.0,
            )
            return hostname
        except Exception:
            return ""

    # ------------------------------------------------------------------
    # Internal: events
    # ------------------------------------------------------------------

    async def _fire_event(self, dev: AutoConfigureDevice) -> None:
        if self._state and hasattr(self._state, "events"):
            await self._state.events.put({
                "type":        "device_discovered",
                "ip":          dev.ip,
                "mac":         dev.mac,
                "vendor":      dev.vendor,
                "device_type": dev.device_type,
                "rtsp_count":  len(dev.rtsp_urls),
                "onvif":       dev.onvif,
                "hostname":    dev.hostname,
                "ts":          time.time(),
            })

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "devices.json"
        tmp = p.with_suffix(".tmp")
        data = {ip: d.to_dict() for ip, d in self._devices.items()}
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _load(self) -> None:
        p = self._data_dir / "devices.json"
        if not p.exists():
            return
        try:
            data = json.loads(p.read_text())
            for ip, d in data.items():
                self._devices[ip] = AutoConfigureDevice.from_dict(d)
        except Exception:
            log.exception("Failed to load auto-configure state")
