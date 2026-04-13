"""Active discovery probes and fingerprint database for network devices."""

import asyncio
import json
import logging
import socket
import struct
from dataclasses import dataclass
from typing import List, Dict, Any, Optional
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
    
    async def probe_mqtt_connect(self, ip: str, port: int = 1883) -> bool:
        """Probe MQTT CONNECT handshake."""
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(ip, port),
                timeout=self.timeout
            )
            # Send CONNECT packet (minimal MQTT CONNECT)
            connect_packet = bytes([
                0x10,  # CONNECT
                0x0C,  # Remaining length
                0x00, 0x04, ord('M'), ord('Q'), ord('T'), ord('T'),  # Protocol name
                0x04,  # Protocol level
                0x02,  # Connect flags
                0x00, 0x3C,  # Keep alive
                0x00, 0x03, ord('c'), ord('l'), ord('i')  # Client ID
            ])
            writer.write(connect_packet)
            await writer.drain()
            
            # Read response
            response = await asyncio.wait_for(reader.read(1024), timeout=self.timeout)
            writer.close()
            await writer.wait_closed()
            
            # Check if we got a CONNACK response (first byte should be 0x20)
            return len(response) > 0 and response[0] == 0x20
        except Exception as e:
            logger.debug(f"MQTT CONNECT probe failed for {ip}:{port}: {e}")
            return False
    
    async def probe_nut_hello(self, ip: str, port: int = 3493) -> bool:
        """Probe NUT HELLO handshake."""
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(ip, port),
                timeout=self.timeout
            )
            # Send HELLO command
            writer.write(b"HELLO\n")
            await writer.drain()
            
            # Read response
            response = await asyncio.wait_for(reader.read(1024), timeout=self.timeout)
            writer.close()
            await writer.wait_closed()
            
            # Check if we got a proper NUT response
            return b"OK" in response or b"NAK" in response
        except Exception as e:
            logger.debug(f"NUT HELLO probe failed for {ip}:{port}: {e}")
            return False
    
    async def probe_device(self, ip: str, spec: ProbeSpec) -> Optional[Dict[str, Any]]:
        """Probe a single device with a specific fingerprint."""
        if spec.method in ['http', 'https']:
            data = await self.probe_http(ip, spec.port, spec.method, spec.path)
            if data:
                # Check if expected keys are present in the actual data structure
                if not spec.expected_keys:
                    # No specific keys required
                    return {
                        "ip": ip,
                        "port": spec.port,
                        "device_type": spec.device_type,
                        "subtype": spec.subtype,
                        "data": data
                    }
                else:
                    # Check if all expected keys are present in the data
                    data_str = json.dumps(data) if isinstance(data, (dict, list)) else str(data)
                    if all(key in data_str for key in spec.expected_keys):
                        return {
                            "ip": ip,
                            "port": spec.port,
                            "device_type": spec.device_type,
                            "subtype": spec.subtype,
                            "data": data
                        }
        elif spec.method == 'tcp':
            # Special handling for specific TCP probes
            if spec.subtype == 'MQTT':
                if await self.probe_mqtt_connect(ip, spec.port):
                    return {
                        "ip": ip,
                        "port": spec.port,
                        "device_type": spec.device_type,
                        "subtype": spec.subtype,
                        "data": {}
                    }
            elif spec.subtype == 'NUT':
                if await self.probe_nut_hello(ip, spec.port):
                    return {
                        "ip": ip,
                        "port": spec.port,
                        "device_type": spec.device_type,
                        "subtype": spec.subtype,
                        "data": {}
                    }
            else:
                # Generic TCP port check
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
    results = []
    try:
        # Create UDP socket for multicast
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
        sock.settimeout(5)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        
        # Enable multicast
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
        
        # WS-Discovery probe message
        probe_message = '''<?xml version="1.0" encoding="utf-8"?>
        <soap:Envelope 
            xmlns:soap="http://www.w3.org/2003/05/soap-envelope" 
            xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" 
            xmlns:tns="http://schemas.xmlsoap.org/ws/2005/04/discovery">
            <soap:Header>
                <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</wsa:Action>
                <wsa:MessageID>uuid:42a55bf9-1853-4540-b6a7-11b2c0545281</wsa:MessageID>
                <wsa:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</wsa:To>
            </soap:Header>
            <soap:Body>
                <tns:Probe/>
            </soap:Body>
        </soap:Envelope>'''
        
        # Send multicast probe
        sock.sendto(probe_message.encode(), ("239.255.255.250", 3702))
        
        # Collect responses
        try:
            while True:
                data, addr = sock.recvfrom(4096)
                # Parse the response
                try:
                    root = ET.fromstring(data)
                    # Extract device info from XML
                    device_info = {
                        "ip": addr[0],
                        "port": addr[1],
                        "device_type": "onvif",
                        "data": str(data)
                    }
                    results.append(device_info)
                except ET.ParseError:
                    continue
        except socket.timeout:
            pass
        finally:
            sock.close()
    except Exception as e:
        logger.debug(f"ONVIF discovery failed: {e}")
    
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
        
        # Check if we got a valid RTSP response with 200 OK
        response_str = response.decode('utf-8', errors='ignore')
        return "RTSP/1.0 200 OK" in response_str or "200 OK" in response_str
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
        
        discovered_devices = []
        
        def on_message(client, userdata, msg):
            try:
                payload = json.loads(msg.payload.decode())
                discovered_devices.append({
                    "topic": msg.topic,
                    "payload": payload
                })
            except Exception as e:
                logger.debug(f"MQTT message parsing failed: {e}")
        
        # Set up message handler
        mqtt_client.on_message = on_message
        
        # Subscribe to topics
        for topic in topics:
            mqtt_client.subscribe(topic)
        
        # Wait for messages with timeout
        await asyncio.sleep(timeout)
        
        # Process discovered devices
        for device in discovered_devices:
            devices.append({
                "device_type": "mqtt_device",
                "topic": device["topic"],
                "data": device["payload"]
            })
            
    except Exception as e:
        logger.error(f"MQTT device enumeration failed: {e}")
    
    return devices
