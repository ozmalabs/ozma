"""Active discovery probes and fingerprint database for network devices."""

import asyncio
import json
import logging
from dataclasses import dataclass
from typing import List, Dict, Any, Optional
from urllib.parse import urljoin
import aiohttp
from xml.etree import ElementTree as ET

logger = logging.getLogger(__name__)

@dataclass
class ProbeSpec:
    """Specification for a discovery probe."""
    port: int
    method: str  # 'http', 'https', 'tcp', 'mqtt', 'rtsp', 'wsd'
    path: str
    expected_keys: List[str]
    device_type: str
    subtype: str = ""

# Fingerprint database for device discovery
FINGERPRINT_DB: List[ProbeSpec] = [
    # Network devices
    ProbeSpec(80, 'http', '/rest/system/routerboard', ['routerboard'], 'network', 'MikroTik'),
    ProbeSpec(8728, 'tcp', '', [], 'network', 'MikroTik-legacy'),
    ProbeSpec(8443, 'https', '/api/system', ['data'], 'network', 'UniFi'),
    ProbeSpec(8043, 'https', '/api/v2/login', [], 'network', 'Omada'),
    ProbeSpec(80, 'http', '/cgi-bin/luci/', [], 'network', 'OpenWrt'),
    ProbeSpec(443, 'https', '/api/core/dashboard', [], 'network', 'pfSense'),
    
    # NAS devices
    ProbeSpec(80, 'http', '/api/v2.0/system/info', ['version'], 'nas', 'TrueNAS'),
    ProbeSpec(443, 'https', '/api/v2.0/system/info', ['version'], 'nas', 'TrueNAS'),
    ProbeSpec(5000, 'http', '/webapi/query.cgi', ['success'], 'nas', 'Synology'),
    ProbeSpec(8080, 'http', '/cgi-bin/authLogin.cgi', ['version'], 'nas', 'QNAP'),
    
    # Virtualization
    ProbeSpec(8006, 'https', '/api2/json/version', ['version'], 'virt', 'Proxmox'),
    ProbeSpec(2375, 'http', '/info', ['Version'], 'virt', 'Docker'),
    ProbeSpec(9000, 'http', '/api/endpoints', [], 'virt', 'Portainer'),
    ProbeSpec(9443, 'https', '/api/endpoints', [], 'virt', 'Portainer'),
    
    # Media servers
    ProbeSpec(32400, 'http', '/identity', ['MediaContainer'], 'media', 'Plex'),
    ProbeSpec(8096, 'http', '/System/Info/Public', ['Id'], 'media', 'Jellyfin'),
    ProbeSpec(8096, 'http', '/emby/System/Info/Public', ['Id'], 'media', 'Emby'),
    ProbeSpec(4533, 'http', '/rest/ping', ['response'], 'media', 'Navidrome'),
    ProbeSpec(13378, 'http', '/api/ping', ['success'], 'media', 'Audiobookshelf'),
    ProbeSpec(2283, 'http', '/api/server-info/ping', ['success'], 'media', 'Immich'),
    
    # Smart home
    ProbeSpec(8123, 'http', '/api/', ['message'], 'smart_home', 'Home Assistant'),
    ProbeSpec(80, 'http', '/api/0/config', ['name'], 'smart_home', 'Hue Bridge'),
    ProbeSpec(80, 'http', '/json/info', ['uptime'], 'smart_home', 'WLED'),
    ProbeSpec(8080, 'http', '/rest/items', [], 'smart_home', 'OpenHAB'),
    ProbeSpec(1880, 'http', '/settings', [], 'smart_home', 'Node-RED'),
    ProbeSpec(5000, 'http', '/api/version', [], 'smart_home', 'Frigate'),
    
    # Other services
    ProbeSpec(1883, 'tcp', '', [], 'other', 'MQTT'),
    ProbeSpec(3493, 'tcp', '', [], 'other', 'NUT'),
    ProbeSpec(22, 'tcp', '', [], 'other', 'SSH'),
    ProbeSpec(5000, 'http', '/api/version', [], 'other', 'OctoPrint'),
    ProbeSpec(7125, 'http', '/printer/info', [], 'other', 'Moonraker'),
    ProbeSpec(80, 'http', '/alive', [], 'other', 'Vaultwarden'),
]

class DiscoveryProber:
    """Active network discovery prober with fingerprint database."""
    
    def __init__(self, timeout: int = 5):
        self.timeout = timeout
        self.session = None
    
    async def __aenter__(self):
        self.session = aiohttp.ClientSession(
            timeout=aiohttp.ClientTimeout(total=self.timeout)
        )
        return self
    
    async def __aexit__(self, exc_type, exc_val, exc_tb):
        if self.session:
            await self.session.close()
    
    async def probe_http(self, ip: str, port: int, method: str, path: str) -> Optional[Dict[str, Any]]:
        """Probe HTTP/HTTPS endpoint."""
        try:
            url = f"{method}://{ip}:{port}{path}"
            async with self.session.get(url, ssl=False) as response:
                if response.status == 200:
                    try:
                        data = await response.json()
                        return data
                    except:
                        text = await response.text()
                        return {"text": text}
        except Exception as e:
            logger.debug(f"HTTP probe failed for {ip}:{port}{path}: {e}")
            return None
    
    async def probe_tcp(self, ip: str, port: int) -> bool:
        """Probe TCP port connectivity."""
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(ip, port),
                timeout=self.timeout
            )
            writer.close()
            await writer.wait_closed()
            return True
        except Exception as e:
            logger.debug(f"TCP probe failed for {ip}:{port}: {e}")
            return False
    
    async def probe_device(self, ip: str, spec: ProbeSpec) -> Optional[Dict[str, Any]]:
        """Probe a single device with a specific fingerprint."""
        if spec.method in ['http', 'https']:
            data = await self.probe_http(ip, spec.port, spec.method, spec.path)
            if data:
                # Check if expected keys are present
                if not spec.expected_keys or all(key in str(data) for key in spec.expected_keys):
                    return {
                        "ip": ip,
                        "port": spec.port,
                        "device_type": spec.device_type,
                        "subtype": spec.subtype,
                        "data": data
                    }
        elif spec.method == 'tcp':
            if await self.probe_tcp(ip, spec.port):
                return {
                    "ip": ip,
                    "port": spec.port,
                    "device_type": spec.device_type,
                    "subtype": spec.subtype,
                    "data": {}
                }
        
        return None
    
    async def scan_host(self, ip: str) -> List[Dict[str, Any]]:
        """Scan a single host with all fingerprints."""
        results = []
        tasks = [self.probe_device(ip, spec) for spec in FINGERPRINT_DB]
        probe_results = await asyncio.gather(*tasks, return_exceptions=True)
        
        for result in probe_results:
            if isinstance(result, dict) and result is not None:
                results.append(result)
        
        return results

async def probe_onvif_discovery(ip_range: List[str]) -> List[Dict[str, Any]]:
    """Probe for ONVIF devices using WS-Discovery."""
    # This would implement multicast UDP probe to 239.255.255.250:3702
    # and parse ProbeMatch responses
    results = []
    # Implementation would go here
    return results

async def probe_rtsp_stream(ip: str, port: int = 554) -> bool:
    """Probe RTSP stream availability."""
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection(ip, port),
            timeout=5
        )
        # Send DESCRIBE request
        describe_request = f"DESCRIBE rtsp://{ip}/ RTSP/1.0\r\nCSeq: 1\r\n\r\n"
        writer.write(describe_request.encode())
        await writer.drain()
        
        # Read response
        response = await asyncio.wait_for(reader.read(1024), timeout=5)
        writer.close()
        await writer.wait_closed()
        
        # Check if we got a 200 OK response
        return b"200 OK" in response
    except Exception as e:
        logger.debug(f"RTSP probe failed for {ip}:{port}: {e}")
        return False

async def probe_mqtt_devices(mqtt_client, timeout: int = 10) -> List[Dict[str, Any]]:
    """Enumerate devices through MQTT broker."""
    devices = []
    try:
        # Subscribe to common IoT discovery topics
        topics = [
            "zigbee2mqtt/bridge/devices",
            "tasmota/discovery/#",
            "esphome/#"
        ]
        
        for topic in topics:
            # This would implement actual MQTT subscription and message parsing
            pass
            
        # Wait for messages with timeout
        await asyncio.sleep(timeout)
        
    except Exception as e:
        logger.error(f"MQTT device enumeration failed: {e}")
    
    return devices
