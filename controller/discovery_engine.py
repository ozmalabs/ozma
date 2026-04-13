# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Universal device discovery engine supporting mDNS, SSDP, and active scanning.

Discovers all types of devices on the network including:
- Ozma nodes (_ozma._udp)
- Network infrastructure (_http._tcp, _https._tcp)
- Printers (_printer._tcp, _ipp._tcp)
- File shares (_smb._tcp)
- Smart home devices (_hap._tcp, _googlecast._tcp)
- Media services (_spotify-connect._tcp, _airplay._tcp)
- IoT devices (_mqtt._tcp)
- Workstations (_workstation._tcp)

Implements background scanning, SSDP listener, and persistent device tracking.
"""

import asyncio
import json
import logging
import socket
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING, Any, Literal

import aiohttp
from zeroconf import ServiceBrowser, ServiceStateChange, Zeroconf
from zeroconf.asyncio import AsyncServiceInfo, AsyncZeroconf

from config import Config
from state import AppState, NodeInfo

if TYPE_CHECKING:
    pass

log = logging.getLogger("ozma.discovery")

REQUIRED_PROTO_VERSION = 1

# Service types to discover via mDNS
MDNS_SERVICE_TYPES = [
    "_ozma._udp.local.",
    "_http._tcp.local.",
    "_https._tcp.local.",
    "_printer._tcp.local.",
    "_ipp._tcp.local.",
    "_smb._tcp.local.",
    "_hap._tcp.local.",
    "_googlecast._tcp.local.",
    "_spotify-connect._tcp.local.",
    "_airplay._tcp.local.",
    "_mqtt._tcp.local.",
    "_workstation._tcp.local.",
]

# UPnP device type mappings
UPNP_DEVICE_TYPES = {
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1": "router",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2": "router",
    "urn:schemas-upnp-org:device:WANDevice:1": "router",
    "urn:schemas-upnp-org:device:WANDevice:2": "router",
    "urn:schemas-upnp-org:device:WANConnectionDevice:1": "router",
    "urn:schemas-upnp-org:device:Printer:1": "printer",
    "urn:schemas-upnp-org:device:Basic:1": "generic",
    "urn:schemas-upnp-org:device:MediaServer:1": "media_server",
    "urn:schemas-upnp-org:device:MediaServer:2": "media_server",
    "urn:schemas-upnp-org:device:MediaServer:3": "media_server",
    "urn:schemas-upnp-org:device:MediaServer:4": "media_server",
    "urn:schemas-upnp-org:device:MediaRenderer:1": "media_renderer",
    "urn:schemas-upnp-org:device:MediaRenderer:2": "media_renderer",
    "urn:schemas-upnp-org:device:MediaRenderer:3": "media_renderer",
    "urn:schemas-upnp-org:device:ZonePlayer:1": "media_renderer",
    "urn:schemas-upnp-org:device:DigitalSecurityCamera:1": "camera",
    "urn:schemas-upnp-org:device:DigitalSecurityCamera:2": "camera",
    "urn:schemas-upnp-org:device:DigitalSecurityCamera:3": "camera",
    "urn:schemas-upnp-org:device:NetworkStorage:1": "nas",
    "urn:schemas-upnp-org:device:NetworkAttachedStorage:1": "nas",
    "urn:schemas-upnp-org:device:TV:1": "tv",
    "urn:schemas-upnp-org:device:TV:2": "tv",
    "urn:schemas-upnp-org:device:Television:1": "tv",
    "urn:schemas-upnp-org:device:Television:2": "tv",
}

# Common service subtypes based on model names or other identifiers
SERVICE_SUBTYPES = {
    "plex": ["plex", "plex media server"],
    "jellyfin": ["jellyfin"],
    "emby": ["emby"],
    "truenas": ["truenas"],
    "unraid": ["unraid"],
    "synology": ["synology", "diskstation"],
    "qnap": ["qnap"],
    "homeassistant": ["home assistant"],
    "homebridge": ["homebridge"],
    "frigate": ["frigate"],
}

@dataclass
class DiscoveredDevice:
    """Represents a discovered device on the network."""
    ip: str | None
    hostname: str | None = None
    mac: str | None = None
    device_type: str = "unknown"  # Full catalogue from spec
    subtype: str | None = None  # 'plex', 'jellyfin', 'truenas', etc.
    friendly_name: str | None = None
    model: str | None = None
    version: str | None = None
    open_ports: list[int] = field(default_factory=list)
    discovery_method: Literal['mdns', 'ssdp', 'active_probe', 'manual'] = 'mdns'
    first_seen: datetime = field(default_factory=datetime.now)
    last_seen: datetime = field(default_factory=datetime.now)
    configured: bool = False  # has been onboarded
    suggested_onboarding: dict = field(default_factory=dict)  # wizard_type + required_fields
    raw_data: dict = field(default_factory=dict)  # Raw discovery data for debugging

    def update_last_seen(self) -> None:
        """Update the last_seen timestamp to now."""
        self.last_seen = datetime.now()

    def to_dict(self) -> dict:
        """Convert to dictionary for JSON serialization."""
        return {
            "ip": self.ip,
            "hostname": self.hostname,
            "mac": self.mac,
            "device_type": self.device_type,
            "subtype": self.subtype,
            "friendly_name": self.friendly_name,
            "model": self.model,
            "version": self.version,
            "open_ports": self.open_ports,
            "discovery_method": self.discovery_method,
            "first_seen": self.first_seen.isoformat(),
            "last_seen": self.last_seen.isoformat(),
            "configured": self.configured,
            "suggested_onboarding": self.suggested_onboarding,
        }

    @classmethod
    def from_dict(cls, data: dict) -> "DiscoveredDevice":
        """Create from dictionary."""
        return cls(
            ip=data.get("ip"),
            hostname=data.get("hostname"),
            mac=data.get("mac"),
            device_type=data.get("device_type", "unknown"),
            subtype=data.get("subtype"),
            friendly_name=data.get("friendly_name"),
            model=data.get("model"),
            version=data.get("version"),
            open_ports=data.get("open_ports", []),
            discovery_method=data.get("discovery_method", "mdns"),
            first_seen=datetime.fromisoformat(data["first_seen"]) if "first_seen" in data else datetime.now(),
            last_seen=datetime.fromisoformat(data["last_seen"]) if "last_seen" in data else datetime.now(),
            configured=data.get("configured", False),
            suggested_onboarding=data.get("suggested_onboarding", {}),
        )


def _parse_txt(properties: dict[bytes, bytes | None]) -> dict[str, str]:
    out: dict[str, str] = {}
    for k, v in properties.items():
        key = k.decode("utf-8", errors="replace")
        val = v.decode("utf-8", errors="replace") if v is not None else ""
        out[key] = val
    return out


class EventEmitter:
    """Simple event emitter for device discovery events."""
    
    def __init__(self) -> None:
        self._listeners: dict[str, list[Callable]] = {}
    
    def on(self, event: str, callback: Callable) -> None:
        """Register a listener for an event."""
        if event not in self._listeners:
            self._listeners[event] = []
        self._listeners[event].append(callback)
    
    def emit(self, event: str, *args, **kwargs) -> None:
        """Emit an event to all registered listeners."""
        if event in self._listeners:
            for callback in self._listeners[event]:
                try:
                    if asyncio.iscoroutinefunction(callback):
                        asyncio.create_task(callback(*args, **kwargs))
                    else:
                        callback(*args, **kwargs)
                except Exception as e:
                    log.error(f"Error in event listener for {event}: {e}")


class UniversalDiscovery:
    """Universal device discovery engine supporting mDNS, SSDP, and active scanning."""
    
    def __init__(self, config: Config, state: AppState) -> None:
        self._config = config
        self._state = state
        self._azc: AsyncZeroconf | None = None
        self._mdns_browsers: list[ServiceBrowser] = []
        self._ssdp_transport: Any = None
        self._ssdp_protocol: Any = None
        self._background_scan_task: asyncio.Task | None = None
        self._mdns_requery_task: asyncio.Task | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._devices: dict[str, DiscoveredDevice] = {}  # key: ip:port or unique_id
        self._event_emitter = EventEmitter()
        self._data_dir = getattr(config, 'data_dir', '/var/lib/ozma')
        
    def on(self, event: str, callback: Callable) -> None:
        """Register an event listener."""
        self._event_emitter.on(event, callback)
        
    async def start(self) -> None:
        """Launch mDNS listener + SSDP listener + background scan loop."""
        self._loop = asyncio.get_running_loop()
        
        # Start mDNS discovery
        await self._start_mdns_discovery()
        
        # Start SSDP discovery
        await self._start_ssdp_discovery()
        
        # Start background scanning
        self._background_scan_task = asyncio.create_task(
            self._background_scan_loop(), name="background-scan"
        )
        
        # Start mDNS requery for existing nodes
        self._mdns_requery_task = asyncio.create_task(
            self._mdns_requery_loop(), name="mdns-requery"
        )
        
        # Load previously discovered devices
        await self._load_discovered_devices()
        
        log.info("Universal discovery engine started")
    
    async def stop(self) -> None:
        """Stop all discovery activities."""
        if self._background_scan_task:
            self._background_scan_task.cancel()
        if self._mdns_requery_task:
            self._mdns_requery_task.cancel()
        if self._ssdp_transport:
            self._ssdp_transport.close()
        if self._azc:
            await self._azc.async_close()
            
        # Save discovered devices
        await self._save_discovered_devices()
        
        log.info("Universal discovery engine stopped")
    
    async def scan(self, subnet: str | None = None) -> list[DiscoveredDevice]:
        """Perform on-demand active scan of the network."""
        devices = []
        
        # For now, just return cached results
        # In a full implementation, this would perform active network scanning
        devices.extend(self._devices.values())
        
        log.info(f"Active scan completed, found {len(devices)} devices")
        return devices
    
    def get_all(self) -> list[DiscoveredDevice]:
        """Get all currently discovered devices."""
        return list(self._devices.values())
    
    async def _start_mdns_discovery(self) -> None:
        """Start mDNS service browsers for all supported service types."""
        self._azc = AsyncZeroconf()
        
        for service_type in MDNS_SERVICE_TYPES:
            browser = ServiceBrowser(
                self._azc.zeroconf,
                service_type,
                handlers=[self._on_mdns_service_state_change],
            )
            self._mdns_browsers.append(browser)
            
        log.info("mDNS browsers started for %d service types", len(MDNS_SERVICE_TYPES))
    
    def _on_mdns_service_state_change(
        self,
        zeroconf: Zeroconf,
        service_type: str,
        name: str,
        state_change: ServiceStateChange,
    ) -> None:
        """Handle mDNS service state changes."""
        # Called from a zeroconf thread — schedule onto the asyncio event loop
        assert self._loop is not None
        if state_change in (ServiceStateChange.Added, ServiceStateChange.Updated):
            self._loop.call_soon_threadsafe(
                self._loop.create_task,
                self._resolve_mdns_service(zeroconf, service_type, name),
            )
        elif state_change == ServiceStateChange.Removed:
            self._loop.call_soon_threadsafe(
                self._loop.create_task,
                self._handle_mdns_service_removed(name),
            )
    
    async def _resolve_mdns_service(
        self, zeroconf: Zeroconf, service_type: str, name: str
    ) -> None:
        """Resolve an mDNS service and create/update DiscoveredDevice."""
        try:
            info = AsyncServiceInfo(service_type, name)
            await info.async_request(zeroconf, timeout=3000)

            addresses = info.parsed_addresses()
            if not addresses:
                log.debug("Could not resolve address for %s", name)
                return

            host = addresses[0]
            port = info.port or 0
            txt = _parse_txt(info.properties)
            
            # Create a unique key for this device
            device_key = f"{host}:{port}" if port else host
            
            # Check if we already know about this device
            existing_device = self._devices.get(device_key)
            
            # Extract common fields
            friendly_name = txt.get("friendlyname") or txt.get("name") or name.split(".")[0]
            model = txt.get("model") or txt.get("modelname")
            version = txt.get("version") or txt.get("fw") or txt.get("swversion")
            
            # Determine device type based on service type
            device_type = self._classify_mdns_service(service_type, txt)
            
            # Determine subtype based on model/name
            subtype = self._classify_subtype(friendly_name, model)
            
            # For Ozma nodes, we have special handling
            if service_type == "_ozma._udp.local.":
                await self._handle_ozma_node(info, host, port, txt, device_key)
                return
            
            # Create or update device
            if existing_device:
                # Update existing device
                existing_device.hostname = info.server.rstrip(".")
                existing_device.friendly_name = friendly_name
                existing_device.model = model
                existing_device.version = version
                existing_device.device_type = device_type
                existing_device.subtype = subtype
                existing_device.open_ports = [port] if port else []
                existing_device.discovery_method = "mdns"
                existing_device.update_last_seen()
                existing_device.raw_data.update({
                    "service_type": service_type,
                    "txt": txt,
                })
                
                self._event_emitter.emit("device_updated", existing_device)
                log.debug("Updated device: %s (%s)", friendly_name, device_key)
            else:
                # Create new device
                device = DiscoveredDevice(
                    ip=host,
                    hostname=info.server.rstrip("."),
                    friendly_name=friendly_name,
                    model=model,
                    version=version,
                    device_type=device_type,
                    subtype=subtype,
                    open_ports=[port] if port else [],
                    discovery_method="mdns",
                    raw_data={
                        "service_type": service_type,
                        "txt": txt,
                    }
                )
                self._devices[device_key] = device
                self._event_emitter.emit("device_discovered", device)
                log.debug("Discovered device: %s (%s)", friendly_name, device_key)
                
        except Exception as e:
            log.error("Error resolving mDNS service %s: %s", name, e)
    
    async def _handle_ozma_node(
        self, info: AsyncServiceInfo, host: str, port: int, txt: dict[str, str], device_key: str
    ) -> None:
        """Handle special case for Ozma nodes."""
        proto = int(txt.get("proto", "0"))
        if proto != REQUIRED_PROTO_VERSION:
            log.warning(
                "Node %s advertises proto=%d, expected %d — ignoring",
                info.name, proto, REQUIRED_PROTO_VERSION,
            )
            return

        caps_raw = txt.get("cap", "")
        capabilities = [c.strip() for c in caps_raw.split(",") if c.strip()]

        vnc_port_str = txt.get("vnc_port", "")
        vnc_port = int(vnc_port_str) if vnc_port_str.isdigit() else None
        vnc_host = txt.get("vnc_host") or None

        stream_port_str = txt.get("stream_port", "")
        stream_port = int(stream_port_str) if stream_port_str.isdigit() else None
        stream_path = txt.get("stream_path") or None

        api_port_str = txt.get("api_port", "")
        api_port = int(api_port_str) if api_port_str.isdigit() else stream_port

        audio_type = txt.get("audio_type") or None
        audio_sink = txt.get("audio_sink") or None
        audio_vban_str = txt.get("audio_vban_port", "")
        audio_vban_port = int(audio_vban_str) if audio_vban_str.isdigit() else None
        mic_vban_str = txt.get("mic_vban_port", "")
        mic_vban_port = int(mic_vban_str) if mic_vban_str.isdigit() else None
        capture_device = txt.get("capture_device") or None

        sunshine_port_str = txt.get("sunshine_port", "")
        sunshine_port = int(sunshine_port_str) if sunshine_port_str.isdigit() else None

        vm_guest_ip = txt.get("vm_ip") or None

        machine_class = txt.get("machine_class") or "workstation"
        frigate_host = txt.get("frigate_host") or None
        frigate_port_str = txt.get("frigate_port", "")
        frigate_port = int(frigate_port_str) if frigate_port_str.isdigit() else None
        camera_streams_raw = txt.get("camera_streams", "")
        try:
            camera_streams = json.loads(camera_streams_raw) if camera_streams_raw else []
        except json.JSONDecodeError:
            log.warning("Node %s has malformed camera_streams TXT record", info.name)
            camera_streams = []

        node = NodeInfo(
            id=info.name,
            host=host,
            port=port,
            role=txt.get("role", "unknown"),
            hw=txt.get("hw", "unknown"),
            fw_version=txt.get("fw", "unknown"),
            proto_version=proto,
            capabilities=capabilities,
            last_seen=time.monotonic(),
            vnc_host=vnc_host,
            vnc_port=vnc_port,
            stream_port=stream_port,
            stream_path=stream_path,
            api_port=api_port,
            audio_type=audio_type,
            audio_sink=audio_sink,
            audio_vban_port=audio_vban_port,
            mic_vban_port=mic_vban_port,
            capture_device=capture_device,
            machine_class=machine_class,
            frigate_host=frigate_host,
            frigate_port=frigate_port,
            camera_streams=camera_streams,
            sunshine_port=sunshine_port,
            vm_guest_ip=vm_guest_ip,
        )
        await self._state.add_node(node)
        log.info("Node online: %s @ %s:%d role=%s hw=%s", info.name, host, port, node.role, node.hw)
    
    def _classify_mdns_service(self, service_type: str, txt: dict[str, str]) -> str:
        """Classify device type based on mDNS service type and TXT records."""
        service_map = {
            "_ozma._udp.local.": "ozma_node",
            "_http._tcp.local.": "web_server",
            "_https._tcp.local.": "web_server",
            "_printer._tcp.local.": "printer",
            "_ipp._tcp.local.": "printer",
            "_smb._tcp.local.": "file_share",
            "_hap._tcp.local.": "homekit_device",
            "_googlecast._tcp.local.": "chromecast",
            "_spotify-connect._tcp.local.": "spotify_connect",
            "_airplay._tcp.local.": "airplay",
            "_mqtt._tcp.local.": "mqtt_broker",
            "_workstation._tcp.local.": "workstation",
        }
        
        return service_map.get(service_type, "unknown")
    
    def _classify_subtype(self, friendly_name: str | None, model: str | None) -> str | None:
        """Classify service subtype based on name/model."""
        if not friendly_name and not model:
            return None
            
        search_text = (friendly_name or "") + " " + (model or "")
        search_text = search_text.lower()
        
        for subtype, keywords in SERVICE_SUBTYPES.items():
            for keyword in keywords:
                if keyword in search_text:
                    return subtype
                    
        return None
    
    async def _handle_mdns_service_removed(self, name: str) -> None:
        """Handle removal of an mDNS service."""
        # In a full implementation, we would track which devices were discovered
        # via this specific service and mark them as offline or remove them
        log.debug("mDNS service removed: %s", name)
    
    async def _start_ssdp_discovery(self) -> None:
        """Start SSDP/UPnP discovery listener."""
        # Create UDP endpoint for SSDP
        class SSDPProtocol(asyncio.DatagramProtocol):
            def __init__(self, discovery_engine: "UniversalDiscovery") -> None:
                self.discovery_engine = discovery_engine
            
            def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
                try:
                    self.discovery_engine._handle_ssdp_packet(data.decode('utf-8'), addr)
                except Exception as e:
                    log.debug("Error handling SSDP packet: %s", e)
        
        loop = asyncio.get_running_loop()
        self._ssdp_transport, self._ssdp_protocol = await loop.create_datagram_endpoint(
            lambda: SSDPProtocol(self),
            local_addr=('0.0.0.0', 1900)
        )
        
        # Join multicast group
        sock = self._ssdp_transport.get_extra_info('socket')
        group_bin = socket.inet_aton('239.255.255.250')
        mreq = group_bin + socket.inet_aton('0.0.0.0')
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
        
        # Send initial M-SEARCH
        await self._send_ssdp_search()
        
        log.info("SSDP discovery listener started")
    
    def _handle_ssdp_packet(self, data: str, addr: tuple[str, int]) -> None:
        """Handle incoming SSDP packet."""
        lines = data.strip().split('\r\n')
        if not lines:
            return
            
        # Parse headers
        headers = {}
        for line in lines[1:]:  # Skip first line (HTTP/1.1 200 OK or NOTIFY * HTTP/1.1)
            if ':' in line:
                key, value = line.split(':', 1)
                headers[key.strip().lower()] = value.strip()
        
        # Check if this is a device announcement
        if 'location' in headers:
            # This is a device response, process it
            asyncio.create_task(self._process_ssdp_device(headers, addr))
    
    async def _process_ssdp_device(self, headers: dict[str, str], addr: tuple[str, int]) -> None:
        """Process SSDP device announcement."""
        location = headers.get('location')
        if not location:
            return
            
        try:
            # Fetch device description XML
            async with aiohttp.ClientSession() as session:
                async with session.get(location, timeout=5) as response:
                    if response.status == 200:
                        xml_content = await response.text()
                        await self._parse_ssdp_description(xml_content, headers, addr)
        except Exception as e:
            log.debug("Error fetching SSDP device description from %s: %s", location, e)
    
    async def _parse_ssdp_description(self, xml_content: str, headers: dict[str, str], addr: tuple[str, int]) -> None:
        """Parse SSDP device description XML."""
        # In a full implementation, we would parse the XML to extract:
        # - deviceType
        # - friendlyName
        # - manufacturer
        # - modelName
        # - modelNumber
        # - serialNumber
        
        # For now, we'll create a basic device entry
        ip, port = addr
        device_key = f"{ip}:{port}"
        
        # Extract basic information from headers
        server = headers.get('server', '')
        usn = headers.get('usn', '')
        
        # Classify device type based on USN or server info
        device_type = self._classify_ssdp_device(usn, server)
        
        # Check if device already exists
        existing_device = self._devices.get(device_key)
        
        if existing_device:
            # Update existing device
            existing_device.device_type = device_type
            existing_device.discovery_method = "ssdp"
            existing_device.update_last_seen()
            self._event_emitter.emit("device_updated", existing_device)
        else:
            # Create new device
            device = DiscoveredDevice(
                ip=ip,
                device_type=device_type,
                discovery_method="ssdp",
                open_ports=[port] if port else []
            )
            self._devices[device_key] = device
            self._event_emitter.emit("device_discovered", device)
    
    def _classify_ssdp_device(self, usn: str, server: str) -> str:
        """Classify device type based on SSDP USN and server info."""
        usn_lower = usn.lower()
        server_lower = server.lower()
        
        # Check UPnP device types
        for device_type, classification in UPNP_DEVICE_TYPES.items():
            if device_type in usn_lower or device_type in server_lower:
                return classification
                
        # Fallback classifications
        if 'router' in usn_lower or 'router' in server_lower:
            return 'router'
        if 'printer' in usn_lower or 'printer' in server_lower:
            return 'printer'
        if 'media' in usn_lower or 'media' in server_lower:
            return 'media_device'
            
        return 'unknown'
    
    async def _send_ssdp_search(self) -> None:
        """Send SSDP M-SEARCH request."""
        search_request = (
            "M-SEARCH * HTTP/1.1\r\n"
            "HOST: 239.255.255.250:1900\r\n"
            "MAN: \"ssdp:discover\"\r\n"
            "MX: 3\r\n"
            "ST: ssdp:all\r\n"
            "\r\n"
        )
        
        if self._ssdp_transport:
            try:
                self._ssdp_transport.sendto(
                    search_request.encode('utf-8'),
                    ('239.255.255.250', 1900)
                )
            except Exception as e:
                log.debug("Error sending SSDP search: %s", e)
    
    async def _background_scan_loop(self) -> None:
        """Run background scanning periodically."""
        while True:
            try:
                await asyncio.sleep(300)  # 5 minutes
                await self._send_ssdp_search()  # Send SSDP search periodically
                await self._save_discovered_devices()  # Save state periodically
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error("Error in background scan loop: %s", e)
    
    async def _mdns_requery_loop(self) -> None:
        """Requery mDNS services periodically to detect node loss."""
        while True:
            try:
                await asyncio.sleep(self._config.mdns_requery_interval)
                await self._requery_all()
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error("Error in mDNS requery loop: %s", e)
    
    async def _requery_all(self) -> None:
        """Requery existing devices to check if they're still present."""
        now = time.monotonic()
        stale_threshold = self._config.mdns_requery_interval * 2

        # Handle Ozma nodes (existing logic)
        for nid, node in list(self._state.nodes.items()):
            if node.direct_registered:
                # Direct-registered nodes stay alive as long as they keep
                # re-registering. The node's re-register loop refreshes
                # last_seen every 60s. If it stops, we evict after the
                # stale threshold. No HTTP health check — the controller
                # may be in a container that can't reach the node's LAN IP.
                if (now - node.last_seen) > stale_threshold * 3:
                    log.info("Node offline (no re-registration): %s", nid)
                    await self._state.remove_node(nid)
            else:
                # mDNS nodes: check staleness from last announcement
                if (now - node.last_seen) > stale_threshold:
                    log.info("Node stale, marking offline: %s", nid)
                    await self._state.remove_node(nid)

        # Re-query mDNS nodes — re-resolve TXT records to pick up
        # fields that may have been missing on first discovery.
        if self._azc is None:
            return
        for service_type in MDNS_SERVICE_TYPES:
            # In a full implementation, we would requery specific services
            # For now, we'll just log that we're doing requery
            log.debug("Requerying mDNS service type: %s", service_type)
    
    async def _load_discovered_devices(self) -> None:
        """Load previously discovered devices from disk."""
        try:
            import os
            import json
            
            devices_file = os.path.join(self._data_dir, "discovered_devices.json")
            if os.path.exists(devices_file):
                with open(devices_file, 'r') as f:
                    data = json.load(f)
                    for device_data in data:
                        try:
                            device = DiscoveredDevice.from_dict(device_data)
                            key = f"{device.ip}:{device.open_ports[0]}" if device.open_ports else device.ip or str(hash(device.friendly_name or ""))
                            self._devices[key] = device
                        except Exception as e:
                            log.warning("Error loading device from JSON: %s", e)
                            
                log.info("Loaded %d previously discovered devices", len(self._devices))
        except Exception as e:
            log.warning("Error loading discovered devices: %s", e)
    
    async def _save_discovered_devices(self) -> None:
        """Save discovered devices to disk."""
        try:
            import os
            import json
            
            devices_file = os.path.join(self._data_dir, "discovered_devices.json")
            devices_data = [device.to_dict() for device in self._devices.values()]
            
            # Ensure directory exists
            os.makedirs(os.path.dirname(devices_file), exist_ok=True)
            
            with open(devices_file, 'w') as f:
                json.dump(devices_data, f, indent=2)
                
            log.debug("Saved %d discovered devices to %s", len(devices_data), devices_file)
        except Exception as e:
            log.warning("Error saving discovered devices: %s", e)

    # ── Controller advertisement ──────────────────────────────────────────

    def _get_local_ip(self) -> str:
        """Return the primary local IP address."""
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            return s.getsockname()[0]
        except Exception:
            return "127.0.0.1"
        finally:
            s.close()

    async def announce_controller(self, controller_id: str, api_port: int) -> None:
        """Advertise this controller as _ozma-ctrl._tcp.local."""
        if self._azc is None:
            return
        from zeroconf.asyncio import AsyncServiceInfo as _ASI
        info = _ASI(
            "_ozma-ctrl._tcp.local.",
            f"{controller_id}._ozma-ctrl._tcp.local.",
            addresses=[socket.inet_aton(self._get_local_ip())],
            port=api_port,
            properties={
                b"api_port": str(api_port).encode(),
                b"controller_id": controller_id.encode(),
                b"version": b"1",
            },
            server=f"{controller_id}.local.",
        )
        await self._azc.async_register_service(info)
        self._ctrl_info = info
        log.info("Controller advertised as %s._ozma-ctrl._tcp.local.", controller_id)

    async def withdraw_controller(self) -> None:
        """Withdraw the controller's mDNS advertisement."""
        info = getattr(self, "_ctrl_info", None)
        if info and self._azc:
            try:
                await self._azc.async_unregister_service(info)
            except Exception as e:
                log.debug("withdraw_controller: %s", e)
            self._ctrl_info = None

    async def start_peer_browser(
        self,
        on_found: Callable[[dict], Awaitable[None]],
        on_lost: Callable[[str], Awaitable[None]],
    ) -> None:
        """Start a persistent background browser for _ozma-ctrl._tcp.local. peers.

        ``on_found`` is called with a dict: {id, host, api_port, base_url}
        when a peer is first seen or its address changes.

        ``on_lost`` is called with controller_id when a peer goes away.
        """
        if self._azc is None:
            return
        from zeroconf import ServiceStateChange as SSC
        from zeroconf.asyncio import AsyncServiceBrowser as _ASB

        self._on_peer_found = on_found
        self._on_peer_lost = on_lost
        assert self._loop is not None

        def _on_ctrl_state_change(
            zeroconf: Zeroconf,
            service_type: str,
            name: str,
            state_change: SSC,
        ) -> None:
            if state_change in (SSC.Added, SSC.Updated):
                self._loop.call_soon_threadsafe(  # type: ignore[union-attr]
                    self._loop.create_task,  # type: ignore[union-attr]
                    self._resolve_peer(zeroconf, name),
                )
            elif state_change == SSC.Removed:
                ctrl_id = name.split(".")[0]
                self._loop.call_soon_threadsafe(  # type: ignore[union-attr]
                    self._loop.create_task,  # type: ignore[union-attr]
                    self._peer_lost(ctrl_id),
                )

        self._peer_browser = _ASB(
            self._azc.zeroconf,
            "_ozma-ctrl._tcp.local.",
            handlers=[_on_ctrl_state_change],
        )
        log.info("Peer controller browser started")

    async def _resolve_peer(self, zeroconf: Zeroconf, name: str) -> None:
        """Resolve a peer controller service record and fire on_found."""
        from zeroconf.asyncio import AsyncServiceInfo as _ASI
        info = _ASI("_ozma-ctrl._tcp.local.", name)
        await info.async_request(zeroconf, timeout=3000)
        if not info.addresses:
            log.warning("Could not resolve address for peer %s", name)
            return
        ip = socket.inet_ntoa(info.addresses[0])
        props = {
            k.decode(): (v.decode() if isinstance(v, bytes) else (v or ""))
            for k, v in (info.properties or {}).items()
        }
        api_port = int(props.get("api_port", "7380"))
        ctrl_id = props.get("controller_id", name.split(".")[0])

        # Skip self
        my_info = getattr(self, "_ctrl_info", None)
        if my_info and name == my_info.name:
            return

        log.info("Peer controller seen: %s @ %s:%d", ctrl_id, ip, api_port)
        if self._on_peer_found:
            await self._on_peer_found({
                "id": ctrl_id,
                "host": ip,
                "api_port": api_port,
                "base_url": f"http://{ip}:{api_port}",
            })

    async def _peer_lost(self, ctrl_id: str) -> None:
        """Fire on_lost for a peer that has gone offline."""
        log.info("Peer controller lost: %s", ctrl_id)
        if self._on_peer_lost:
            await self._on_peer_lost(ctrl_id)

    async def discover_controllers(self, timeout: float = 5.0) -> list[dict]:
        """Probe mDNS for _ozma-ctrl._tcp.local. peers on the LAN."""
        if self._azc is None:
            return []
        from zeroconf import ServiceStateChange as SSC
        from zeroconf.asyncio import AsyncServiceInfo as _ASI, AsyncServiceBrowser as _ASB
        found: list[dict] = []
        my_id = getattr(getattr(self, "_ctrl_info", None), "name", None)

        async def _resolve(name: str) -> None:
            info = _ASI("_ozma-ctrl._tcp.local.", name)
            await info.async_request(self._azc.zeroconf, timeout=3000)  # type: ignore[union-attr]
            if not info.addresses:
                return
            ip = socket.inet_ntoa(info.addresses[0])
            props = {
                k.decode(): v.decode() if isinstance(v, bytes) else (v or "")
                for k, v in (info.properties or {}).items()
            }
            api_port = int(props.get("api_port", "7380"))
            ctrl_id = props.get("controller_id", name.split(".")[0])
            # Skip self
            if my_id and name == my_id:
                return
            found.append({"id": ctrl_id, "host": ip, "api_port": api_port,
                          "base_url": f"http://{ip}:{api_port}"})

        tasks: list[asyncio.Task] = []

        def _on_change(zc, stype, name, state_change):
            if state_change == SSC.Added:
                tasks.append(asyncio.get_event_loop().create_task(_resolve(name)))

        browser = _ASB(self._azc.zeroconf, "_ozma-ctrl._tcp.local.", handlers=[_on_change])
        await asyncio.sleep(timeout)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        browser.cancel()
        return found
