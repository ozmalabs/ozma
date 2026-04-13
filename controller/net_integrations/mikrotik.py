# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MikroTik RouterOS integration — full bridge VLAN filtering support.

Supports both RouterOS v7+ REST API and v6 API via port 8728 (librouteros).
RB5009UG+S+ and CRS series switches use bridge VLAN filtering, not legacy
/interface/vlan interfaces.

Transport abstraction handles both REST and API fallback automatically.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import time
from typing import Any, Dict, List, Optional, Union
from dataclasses import dataclass

import httpx

from .base import (
    NetworkBackend, NetworkTopology, NetworkVLAN, NetworkDevice, 
    SwitchPort, DHCPLease, ProvisionResult, PortMode, VLANSpec
)

log = logging.getLogger("ozma.net.mikrotik")

# REST API paths
REST_BASE = "/rest"
REST_SYSTEM_IDENTITY = f"{REST_BASE}/system/identity"
REST_INTERFACES = f"{REST_BASE}/interface"
REST_BRIDGE = f"{REST_INTERFACES}/bridge"
REST_BRIDGE_VLAN = f"{REST_BRIDGE}/vlan"
REST_BRIDGE_PORT = f"{REST_BRIDGE}/port"
REST_IP_ADDRESS = f"{REST_BASE}/ip/address"
REST_DHCP_LEASE = f"{REST_BASE}/ip/dhcp-server/lease"
REST_DHCP_SERVER = f"{REST_BASE}/ip/dhcp-server"
REST_DHCP_POOL = f"{REST_BASE}/ip/pool"
REST_DHCP_NETWORK = f"{REST_BASE}/ip/dhcp-server/network"
REST_DNS_STATIC = f"{REST_BASE}/ip/dns/static"
REST_FIREWALL = f"{REST_BASE}/ip/firewall"
REST_NAT = f"{REST_BASE}/ip/firewall/nat"
REST_ROUTE = f"{REST_BASE}/ip/route"
REST_WIREGUARD = f"{REST_INTERFACES}/wireguard"
REST_WIREGUARD_PEERS = f"{REST_WIREGUARD}/peers"
REST_BRIDGE_HOST = f"{REST_BRIDGE}/host"
REST_SYSTEM_RESOURCE = f"{REST_BASE}/system/resource"

# API sentence prefixes for v6 fallback
API_PREFIXES = {
    REST_BRIDGE: "/interface/bridge",
    REST_BRIDGE_VLAN: "/interface/bridge/vlan",
    REST_BRIDGE_PORT: "/interface/bridge/port",
    REST_IP_ADDRESS: "/ip/address",
    REST_DHCP_LEASE: "/ip/dhcp-server/lease",
    REST_DHCP_SERVER: "/ip/dhcp-server",
    REST_DHCP_POOL: "/ip/pool",
    REST_DHCP_NETWORK: "/ip/dhcp-server/network",
    REST_DNS_STATIC: "/ip/dns/static",
    REST_FIREWALL: "/ip/firewall/filter",
    REST_NAT: "/ip/firewall/nat",
    REST_ROUTE: "/ip/route",
    REST_WIREGUARD: "/interface/wireguard",
    REST_WIREGUARD_PEERS: "/interface/wireguard/peers",
    REST_BRIDGE_HOST: "/interface/bridge/host",
    REST_SYSTEM_IDENTITY: "/system/identity",
    REST_SYSTEM_RESOURCE: "/system/resource",
}

def _expand_vlan_ids(vlan_ids: str) -> List[int]:
    """Expand VLAN ID ranges like '20-25' or '10,20-25,30' into a list of integers."""
    result = []
    for part in vlan_ids.split(','):
        part = part.strip()
        if '-' in part:
            start, end = part.split('-')
            result.extend(range(int(start), int(end) + 1))
        else:
            result.append(int(part))
    return result

class _RestTransport:
    """REST API transport using httpx."""
    
    def __init__(self, host: str, username: str, password: str, 
                 verify_ssl: Union[bool, str] = True):
        self.host = host.rstrip('/')
        self.auth = (username, password)
        self.verify_ssl = verify_ssl
        self.base_url = f"http://{host}"
        
    async def call(self, method: str, path: str, data: Optional[Dict] = None) -> Any:
        """Make REST API call."""
        url = self.base_url + path
        try:
            async with httpx.AsyncClient(auth=self.auth, verify=self.verify_ssl) as client:
                if method == "GET":
                    response = await client.get(url)
                elif method == "POST":
                    response = await client.post(url, json=data)
                elif method == "PUT":
                    response = await client.put(url, json=data)
                elif method == "PATCH":
                    response = await client.patch(url, json=data)
                elif method == "DELETE":
                    response = await client.delete(url)
                else:
                    raise ValueError(f"Unsupported method: {method}")
                
                if response.status_code >= 400:
                    log.warning("REST API call failed: %s %s -> %d", 
                              method, path, response.status_code)
                    return None
                    
                if response.content:
                    return response.json()
                return {}
        except Exception as e:
            log.debug("REST API call failed: %s %s: %s", method, path, e)
            return None

class _ApiTransport:
    """API transport using librouteros for v6 fallback."""
    
    def __init__(self, host: str, username: str, password: str):
        self.host = host
        self.username = username
        self.password = password
        self.connection = None
        
    async def _connect(self):
        """Establish connection if not already connected."""
        if self.connection is None:
            try:
                import librouteros
                self.connection = await librouteros.async_connect(
                    self.host, self.username, self.password, port=8728
                )
            except ImportError:
                log.error("librouteros not installed - cannot connect to RouterOS v6")
                raise
            except Exception as e:
                log.warning("Failed to connect to RouterOS API: %s", e)
                raise
                
    def _api_path(self, rest_path: str) -> str:
        """Convert REST path to API path."""
        for prefix, api_prefix in API_PREFIXES.items():
            if rest_path.startswith(prefix):
                return api_prefix
        return rest_path
        
    async def call(self, method: str, path: str, data: Optional[Dict] = None) -> Any:
        """Make API call via librouteros."""
        if self.connection is None:
            await self._connect()
            
        api_path = self._api_path(path)
        try:
            if method == "GET":
                result = list(await self.connection.async_api(cmd=f"{api_path}/print"))
                return result
            elif method == "POST" or method == "PUT":
                # PUT/POST in RouterOS API are both handled via 'add'
                cmd = f"{api_path}/add"
                result = await self.connection.async_api(cmd=cmd, **(data or {}))
                return {"ret": result} if result else {}
            elif method == "PATCH":
                # PATCH in RouterOS API is 'set' with .id
                if data and ".id" in data:
                    cmd = f"{api_path}/set"
                    result = await self.connection.async_api(cmd=cmd, **data)
                    return result
                else:
                    log.warning("PATCH requires .id in data")
                    return None
            elif method == "DELETE":
                # DELETE in RouterOS API requires .id
                if data and ".id" in data:
                    cmd = f"{api_path}/remove"
                    await self.connection.async_api(cmd=cmd, **{"numbers": data[".id"]})
                    return {}
                else:
                    log.warning("DELETE requires .id in data")
                    return None
        except Exception as e:
            log.debug("API call failed: %s %s: %s", method, path, e)
            return None
            
    async def close(self):
        """Close connection."""
        if self.connection:
            try:
                self.connection.close()
            except:
                pass
            self.connection = None

class MikroTikClient(NetworkBackend):
    """MikroTik RouterOS client with bridge VLAN filtering support."""
    
    name = "mikrotik"
    
    def __init__(self, host: str, username: str, password: str, 
                 verify_ssl: Union[bool, str] = True, transport: str = "auto"):
        self.host = host
        self.username = username
        self.password = password
        self.verify_ssl = verify_ssl
        self.transport_mode = transport
        self._transport = None
        self._bridge_name = "bridge1"  # Default bridge name
        
    async def _detect_transport(self):
        """Auto-detect transport method (REST vs API)."""
        if self._transport:
            return self._transport
            
        if self.transport_mode == "rest":
            self._transport = _RestTransport(self.host, self.username, self.password, self.verify_ssl)
            return self._transport
        elif self.transport_mode == "api":
            self._transport = _ApiTransport(self.host, self.username, self.password)
            return self._transport
            
        # Auto-detect: try REST first
        try:
            rest_transport = _RestTransport(self.host, self.username, self.password, self.verify_ssl)
            result = await rest_transport.call("GET", REST_SYSTEM_IDENTITY)
            if result is not None:
                self._transport = rest_transport
                log.info("Using REST API transport for MikroTik")
                return self._transport
        except:
            pass
            
        # Fall back to API
        try:
            api_transport = _ApiTransport(self.host, self.username, self.password)
            # Test connection
            await api_transport.call("GET", REST_SYSTEM_IDENTITY)
            self._transport = api_transport
            log.info("Using API transport for MikroTik (v6 fallback)")
            return self._transport
        except Exception as e:
            log.error("Failed to detect MikroTik transport: %s", e)
            raise
            
    async def _call(self, method: str, path: str, data: Optional[Dict] = None) -> Any:
        """Make API call through detected transport."""
        transport = await self._detect_transport()
        return await transport.call(method, path, data)
        
    async def get_topology(self) -> NetworkTopology:
        """Get full network topology."""
        try:
            # Get bridge information
            bridges = await self._call("GET", REST_BRIDGE)
            if not bridges:
                return NetworkTopology()
                
            # Find the main bridge
            bridge = next((b for b in bridges if b.get("name") == self._bridge_name), bridges[0])
            bridge_id = bridge.get(".id", "")
            
            # Get bridge ports
            bridge_ports = await self._call("GET", REST_BRIDGE_PORT)
            if not bridge_ports:
                bridge_ports = []
                
            # Get bridge VLAN entries
            bridge_vlans = await self._call("GET", REST_BRIDGE_VLAN)
            if not bridge_vlans:
                bridge_vlans = []
                
            # Get IP addresses
            ip_addresses = await self._call("GET", REST_IP_ADDRESS)
            if not ip_addresses:
                ip_addresses = []
                
            # Get DHCP leases
            dhcp_leases = await self._call("GET", REST_DHCP_LEASE)
            if not dhcp_leases:
                dhcp_leases = []
                
            # Get bridge hosts (connected MACs)
            bridge_hosts = await self._call("GET", REST_BRIDGE_HOST)
            if not bridge_hosts:
                bridge_hosts = []
                
            # Build VLAN list
            vlans = []
            vlan_dict = {}  # vlan_id -> NetworkVLAN
            
            # Process bridge VLAN entries to extract VLAN info
            for bvlan in bridge_vlans:
                vlan_ids_str = bvlan.get("vlan-ids", "")
                if not vlan_ids_str:
                    continue
                    
                vlan_ids = _expand_vlan_ids(vlan_ids_str)
                for vlan_id in vlan_ids:
                    if vlan_id not in vlan_dict:
                        vlan_dict[vlan_id] = NetworkVLAN(
                            vlan_id=vlan_id,
                            name=f"VLAN {vlan_id}",
                            subnet="",  # Will be filled from IP addresses
                            gateway="",  # Will be filled from IP addresses
                            dhcp_enabled=False  # Will be filled from DHCP servers
                        )
                        
            # Match IP addresses to VLANs
            for ip_addr in ip_addresses:
                interface = ip_addr.get("interface", "")
                address = ip_addr.get("address", "")
                if interface.startswith(f"{self._bridge_name}."):
                    # This is a VLAN interface
                    try:
                        vlan_id = int(interface.split(".")[-1])
                        if vlan_id in vlan_dict:
                            vlan_dict[vlan_id].subnet = address
                            # Extract gateway (first IP in subnet)
                            if "/" in address:
                                network, prefix = address.split("/")
                                if "." in network:
                                    parts = network.split(".")
                                    if len(parts) == 4:
                                        parts[-1] = "1"
                                        vlan_dict[vlan_id].gateway = ".".join(parts)
                    except:
                        pass
                        
            # Match DHCP servers to VLANs
            dhcp_servers = await self._call("GET", REST_DHCP_SERVER)
            if dhcp_servers:
                for server in dhcp_servers:
                    interface = server.get("interface", "")
                    if interface.startswith(f"{self._bridge_name}."):
                        try:
                            vlan_id = int(interface.split(".")[-1])
                            if vlan_id in vlan_dict:
                                vlan_dict[vlan_id].dhcp_enabled = True
                        except:
                            pass
                            
            vlans = list(vlan_dict.values())
            
            # Build port list
            ports = []
            # Create a mapping of port interface to MAC/hostname for connected devices
            port_hosts = {}
            for host in bridge_hosts:
                port = host.get("port", "")
                if port:
                    mac = host.get("mac-address", "")
                    hostname = host.get("host-name", "")
                    if port not in port_hosts:
                        port_hosts[port] = []
                    port_hosts[port].append({"mac": mac, "hostname": hostname})
                    
            for bport in bridge_ports:
                port_id = bport.get("interface", "")
                pvid = bport.get("pvid", 1)
                
                # Get connected hosts for this port
                connected = port_hosts.get(port_id, [])
                connected_mac = connected[0]["mac"] if connected else ""
                connected_hostname = connected[0]["hostname"] if connected else ""
                
                ports.append(SwitchPort(
                    port_id=port_id,
                    mode=PortMode.ACCESS,  # Simplified - could be more detailed
                    native_vlan=pvid,
                    tagged_vlans=[],  # Would need to parse from bridge VLAN table
                    connected_mac=connected_mac,
                    connected_hostname=connected_hostname,
                    link_up=True,  # Assume up for now
                    speed_mbps=1000  # Default - could query actual speed
                ))
                
            # Create single device entry for the router/switch
            device = NetworkDevice(
                device_id=bridge_id,
                name=bridge.get("name", "bridge1"),
                model="MikroTik Router",  # Could query actual model
                ip=self.host,
                ports=ports
            )
            
            # Convert DHCP leases
            leases = []
            for lease in dhcp_leases:
                leases.append(DHCPLease(
                    mac=lease.get("mac-address", ""),
                    ip=lease.get("address", ""),
                    hostname=lease.get("host-name", ""),
                    expires=lease.get("expires-after", ""),
                    is_static=lease.get("dynamic", False) is False
                ))
                
            return NetworkTopology(
                vlans=vlans,
                devices=[device],
                leases=leases
            )
        except Exception as e:
            log.warning("Failed to get MikroTik topology: %s", e)
            return NetworkTopology()
            
    async def ensure_vlan(self, spec: VLANSpec) -> ProvisionResult:
        """Ensure VLAN exists with specified configuration."""
        changes = []
        errors = []
        skipped = []
        
        try:
            # Check if VLAN already exists in bridge
            bridge_vlans = await self._call("GET", REST_BRIDGE_VLAN)
            if bridge_vlans:
                for bvlan in bridge_vlans:
                    vlan_ids_str = bvlan.get("vlan-ids", "")
                    if vlan_ids_str:
                        vlan_ids = _expand_vlan_ids(vlan_ids_str)
                        if spec.vlan_id in vlan_ids:
                            # VLAN exists, check if it's on the right bridge
                            if bvlan.get("bridge", "") == self._bridge_name:
                                skipped.append(f"VLAN {spec.vlan_id} already exists on bridge")
                                break
                            
            # If VLAN doesn't exist, create it
            if not skipped:
                # Add VLAN to bridge
                result = await self._call("PUT", REST_BRIDGE_VLAN, {
                    "bridge": self._bridge_name,
                    "vlan-ids": str(spec.vlan_id),
                    "tagged": self._bridge_name  # Tagged on the bridge interface
                })
                if result is not None:
                    changes.append(f"Added VLAN {spec.vlan_id} to bridge {self._bridge_name}")
                else:
                    errors.append(f"Failed to add VLAN {spec.vlan_id} to bridge")
                    
            # Ensure VLAN interface exists for IP configuration
            vlan_interface_name = f"{self._bridge_name}.{spec.vlan_id}"
            ip_addresses = await self._call("GET", REST_IP_ADDRESS)
            vlan_interface_exists = False
            if ip_addresses:
                for ip_addr in ip_addresses:
                    if ip_addr.get("interface", "") == vlan_interface_name:
                        vlan_interface_exists = True
                        break
                        
            if not vlan_interface_exists and spec.gateway:
                # Create VLAN interface
                result = await self._call("PUT", REST_INTERFACES, {
                    "type": "vlan",
                    "name": vlan_interface_name,
                    "vlan-id": spec.vlan_id,
                    "interface": self._bridge_name
                })
                if result is not None:
                    changes.append(f"Created VLAN interface {vlan_interface_name}")
                else:
                    errors.append(f"Failed to create VLAN interface {vlan_interface_name}")
                    
                # Assign IP address
                if spec.gateway:
                    result = await self._call("PUT", REST_IP_ADDRESS, {
                        "address": spec.gateway,
                        "interface": vlan_interface_name
                    })
                    if result is not None:
                        changes.append(f"Assigned IP {spec.gateway} to {vlan_interface_name}")
                    else:
                        errors.append(f"Failed to assign IP to {vlan_interface_name}")
                        
            # Configure DHCP if enabled
            if spec.dhcp_enabled:
                dhcp_result = await self._ensure_dhcp(spec)
                if dhcp_result.changes:
                    changes.extend(dhcp_result.changes)
                if dhcp_result.errors:
                    errors.extend(dhcp_result.errors)
                if dhcp_result.skipped:
                    skipped.extend(dhcp_result.skipped)
                    
            return ProvisionResult(
                success=len(errors) == 0,
                changes=changes,
                errors=errors,
                skipped=skipped
            )
        except Exception as e:
            log.warning("Failed to ensure VLAN %d: %s", spec.vlan_id, e)
            return ProvisionResult(
                success=False,
                errors=[f"Exception ensuring VLAN {spec.vlan_id}: {e}"]
            )
            
    async def _ensure_dhcp(self, spec: VLANSpec) -> ProvisionResult:
        """Ensure DHCP is configured for VLAN."""
        changes = []
        errors = []
        skipped = []
        
        try:
            vlan_interface_name = f"{self._bridge_name}.{spec.vlan_id}"
            
            # Check if DHCP pool exists
            pools = await self._call("GET", REST_DHCP_POOL)
            pool_exists = False
            pool_name = f"dhcp-vlan{spec.vlan_id}"
            
            if pools:
                for pool in pools:
                    if pool.get("name", "") == pool_name:
                        pool_exists = True
                        break
                        
            if not pool_exists and spec.dhcp_start and spec.dhcp_end:
                result = await self._call("PUT", REST_DHCP_POOL, {
                    "name": pool_name,
                    "ranges": f"{spec.dhcp_start}-{spec.dhcp_end}"
                })
                if result is not None:
                    changes.append(f"Created DHCP pool {pool_name}")
                else:
                    errors.append(f"Failed to create DHCP pool {pool_name}")
                    
            # Check if DHCP server exists
            dhcp_servers = await self._call("GET", REST_DHCP_SERVER)
            server_exists = False
            
            if dhcp_servers:
                for server in dhcp_servers:
                    if server.get("interface", "") == vlan_interface_name:
                        server_exists = True
                        break
                        
            if not server_exists and pool_exists:
                result = await self._call("PUT", REST_DHCP_SERVER, {
                    "name": f"dhcp-server-vlan{spec.vlan_id}",
                    "interface": vlan_interface_name,
                    "address-pool": pool_name,
                    "disabled": False
                })
                if result is not None:
                    changes.append(f"Created DHCP server for {vlan_interface_name}")
                else:
                    errors.append(f"Failed to create DHCP server for {vlan_interface_name}")
                    
            # Configure DHCP network (for DNS, gateway, etc.)
            dhcp_networks = await self._call("GET", REST_DHCP_NETWORK)
            network_exists = False
            
            if dhcp_networks:
                for network in dhcp_networks:
                    if network.get("address", "") == spec.subnet:
                        network_exists = True
                        break
                        
            if not network_exists and spec.subnet:
                gateway_ip = spec.gateway.split("/")[0] if "/" in spec.gateway else spec.gateway
                # Extract network part from gateway
                if "." in gateway_ip:
                    parts = gateway_ip.split(".")
                    if len(parts) == 4:
                        parts[-1] = "0"
                        network_addr = ".".join(parts) + "/24"  # Assuming /24
                    else:
                        network_addr = spec.subnet
                else:
                    network_addr = spec.subnet
                    
                result = await self._call("PUT", REST_DHCP_NETWORK, {
                    "address": network_addr,
                    "gateway": gateway_ip,
                    "dns-server": gateway_ip  # Use gateway as DNS
                })
                if result is not None:
                    changes.append(f"Configured DHCP network {network_addr}")
                else:
                    errors.append(f"Failed to configure DHCP network {network_addr}")
                    
            return ProvisionResult(
                success=len(errors) == 0,
                changes=changes,
                errors=errors,
                skipped=skipped
            )
        except Exception as e:
            log.warning("Failed to ensure DHCP for VLAN %d: %s", spec.vlan_id, e)
            return ProvisionResult(
                success=False,
                errors=[f"Exception ensuring DHCP for VLAN {spec.vlan_id}: {e}"]
            )
            
    async def assign_port(self, device_id: str, port_id: str, mode: PortMode,
                         native_vlan: int, tagged_vlans: List[int]) -> ProvisionResult:
        """Assign port to VLAN(s)."""
        changes = []
        errors = []
        skipped = []
        
        try:
            # Get current bridge port configuration
            bridge_ports = await self._call("GET", REST_BRIDGE_PORT)
            port_entry = None
            port_internal_id = None
            
            if bridge_ports:
                for bport in bridge_ports:
                    if bport.get("interface", "") == port_id:
                        port_entry = bport
                        port_internal_id = bport.get(".id", "")
                        break
                        
            # Update port PVID if needed
            if port_entry and port_entry.get("pvid", 0) != native_vlan:
                result = await self._call("PATCH", f"{REST_BRIDGE_PORT}/{port_internal_id}", {
                    ".id": port_internal_id,
                    "pvid": native_vlan
                })
                if result is not None:
                    changes.append(f"Set PVID to {native_vlan} on port {port_id}")
                else:
                    errors.append(f"Failed to set PVID on port {port_id}")
                    
            elif not port_entry:
                # Create new bridge port entry
                result = await self._call("PUT", REST_BRIDGE_PORT, {
                    "interface": port_id,
                    "bridge": self._bridge_name,
                    "pvid": native_vlan
                })
                if result is not None:
                    changes.append(f"Added port {port_id} to bridge with PVID {native_vlan}")
                else:
                    errors.append(f"Failed to add port {port_id} to bridge")
                    
            # Update bridge VLAN configuration for tagged VLANs
            if tagged_vlans:
                bridge_vlans = await self._call("GET", REST_BRIDGE_VLAN)
                vlan_updated = False
                
                if bridge_vlans:
                    for bvlan in bridge_vlans:
                        vlan_ids_str = bvlan.get("vlan-ids", "")
                        if vlan_ids_str:
                            vlan_ids = _expand_vlan_ids(vlan_ids_str)
                            for vlan_id in tagged_vlans:
                                if vlan_id in vlan_ids and bvlan.get("bridge", "") == self._bridge_name:
                                    # Check if port is already tagged
                                    tagged_ports = bvlan.get("tagged", "").split(",")
                                    if port_id not in tagged_ports:
                                        # Add port to tagged list
                                        tagged_ports.append(port_id)
                                        tagged_str = ",".join(tagged_ports)
                                        
                                        bvlan_id = bvlan.get(".id", "")
                                        result = await self._call("PATCH", f"{REST_BRIDGE_VLAN}/{bvlan_id}", {
                                            ".id": bvlan_id,
                                            "tagged": tagged_str
                                        })
                                        if result is not None:
                                            changes.append(f"Added port {port_id} to tagged VLAN {vlan_id}")
                                        else:
                                            errors.append(f"Failed to add port to tagged VLAN {vlan_id}")
                                        vlan_updated = True
                                        
                # If VLAN not found, create it
                if not vlan_updated:
                    for vlan_id in tagged_vlans:
                        result = await self._call("PUT", REST_BRIDGE_VLAN, {
                            "bridge": self._bridge_name,
                            "vlan-ids": str(vlan_id),
                            "tagged": port_id
                        })
                        if result is not None:
                            changes.append(f"Created tagged VLAN {vlan_id} with port {port_id}")
                        else:
                            errors.append(f"Failed to create tagged VLAN {vlan_id}")
                            
            return ProvisionResult(
                success=len(errors) == 0,
                changes=changes,
                errors=errors,
                skipped=skipped
            )
        except Exception as e:
            log.warning("Failed to assign port %s: %s", port_id, e)
            return ProvisionResult(
                success=False,
                errors=[f"Exception assigning port {port_id}: {e}"]
            )
            
    async def get_dhcp_leases(self) -> List[DHCPLease]:
        """Get all DHCP leases."""
        try:
            leases_data = await self._call("GET", REST_DHCP_LEASE)
            if not leases_data:
                return []
                
            leases = []
            for lease in leases_data:
                leases.append(DHCPLease(
                    mac=lease.get("mac-address", ""),
                    ip=lease.get("address", ""),
                    hostname=lease.get("host-name", ""),
                    expires=lease.get("expires-after", ""),
                    is_static=lease.get("dynamic", True) is False
                ))
            return leases
        except Exception as e:
            log.warning("Failed to get DHCP leases: %s", e)
            return []
            
    async def set_dhcp_reservation(self, mac: str, ip: str, hostname: str) -> bool:
        """Set static DHCP reservation."""
        try:
            # Check if reservation already exists
            leases = await self.get_dhcp_leases()
            for lease in leases:
                if lease.mac.lower() == mac.lower() and not lease.is_static:
                    # Convert dynamic lease to static
                    lease_id = None
                    # Would need to find the internal lease ID to modify
                    # For now, just create a new static lease
                    break
                    
            # Create new static lease
            result = await self._call("PUT", REST_DHCP_LEASE, {
                "mac-address": mac,
                "address": ip,
                "host-name": hostname,
                "dynamic": False
            })
            return result is not None
        except Exception as e:
            log.warning("Failed to set DHCP reservation for %s: %s", mac, e)
            return False
            
    # MikroTik-extended methods
            
    async def get_wireguard_interfaces(self) -> List[Dict]:
        """Get all WireGuard interfaces."""
        try:
            return await self._call("GET", REST_WIREGUARD) or []
        except Exception as e:
            log.warning("Failed to get WireGuard interfaces: %s", e)
            return []
            
    async def get_wireguard_peers(self) -> List[Dict]:
        """Get all WireGuard peers."""
        try:
            return await self._call("GET", REST_WIREGUARD_PEERS) or []
        except Exception as e:
            log.warning("Failed to get WireGuard peers: %s", e)
            return []
            
    async def add_wireguard_peer(self, iface: str, pubkey: str, allowed_ips: List[str],
                                endpoint: str = "", comment: str = "") -> bool:
        """Add WireGuard peer."""
        try:
            # Check for duplicate
            peers = await self.get_wireguard_peers()
            for peer in peers:
                if peer.get("public-key", "") == pubkey:
                    log.info("WireGuard peer with pubkey %s already exists", pubkey[:8])
                    return True
                    
            data = {
                "interface": iface,
                "public-key": pubkey,
                "allowed-address": ",".join(allowed_ips),
            }
            if endpoint:
                data["endpoint-address"] = endpoint
            if comment:
                data["comment"] = comment
                
            result = await self._call("PUT", REST_WIREGUARD_PEERS, data)
            return result is not None
        except Exception as e:
            log.warning("Failed to add WireGuard peer: %s", e)
            return False
            
    async def remove_wireguard_peer(self, pubkey: str) -> bool:
        """Remove WireGuard peer by public key."""
        try:
            peers = await self.get_wireguard_peers()
            for peer in peers:
                if peer.get("public-key", "") == pubkey:
                    peer_id = peer.get(".id", "")
                    if peer_id:
                        result = await self._call("DELETE", f"{REST_WIREGUARD_PEERS}/{peer_id}", {".id": peer_id})
                        return result is not None
            return False
        except Exception as e:
            log.warning("Failed to remove WireGuard peer: %s", e)
            return False
            
    async def get_firewall_rules(self, chain: str = "forward") -> List[Dict]:
        """Get firewall rules for specified chain."""
        try:
            rules = await self._call("GET", f"{REST_FIREWALL}?chain={chain}")
            return rules or []
        except Exception as e:
            log.warning("Failed to get firewall rules: %s", e)
            return []
            
    async def ensure_firewall_rule(self, rule: Dict, tag: str) -> bool:
        """Ensure firewall rule exists with specified tag."""
        try:
            # Check for existing rule with this tag
            rules = await self.get_firewall_rules(rule.get("chain", "forward"))
            for existing_rule in rules:
                if existing_rule.get("comment", "") == tag:
                    # Rule exists, could check if it matches
                    return True
                    
            # Add tag to rule
            rule["comment"] = tag
            result = await self._call("PUT", REST_FIREWALL, rule)
            return result is not None
        except Exception as e:
            log.warning("Failed to ensure firewall rule: %s", e)
            return False
            
    async def remove_firewall_rules_by_tag(self, tag: str) -> int:
        """Remove firewall rules with specified tag. Returns count removed."""
        try:
            removed = 0
            rules = await self.get_firewall_rules()
            for rule in rules:
                if rule.get("comment", "") == tag:
                    rule_id = rule.get(".id", "")
                    if rule_id:
                        result = await self._call("DELETE", f"{REST_FIREWALL}/{rule_id}", {".id": rule_id})
                        if result is not None:
                            removed += 1
            return removed
        except Exception as e:
            log.warning("Failed to remove firewall rules by tag: %s", e)
            return 0
            
    async def get_nat_rules(self) -> List[Dict]:
        """Get all NAT rules."""
        try:
            return await self._call("GET", REST_NAT) or []
        except Exception as e:
            log.warning("Failed to get NAT rules: %s", e)
            return []
            
    async def ensure_nat_rule(self, rule: Dict, tag: str) -> bool:
        """Ensure NAT rule exists with specified tag."""
        try:
            # Check for existing rule with this tag
            rules = await self.get_nat_rules()
            for existing_rule in rules:
                if existing_rule.get("comment", "") == tag:
                    # Rule exists
                    return True
                    
            # Add tag to rule
            rule["comment"] = tag
            result = await self._call("PUT", REST_NAT, rule)
            return result is not None
        except Exception as e:
            log.warning("Failed to ensure NAT rule: %s", e)
            return False
            
    async def get_dns_static(self) -> List[Dict]:
        """Get all static DNS entries."""
        try:
            return await self._call("GET", REST_DNS_STATIC) or []
        except Exception as e:
            log.warning("Failed to get static DNS entries: %s", e)
            return []
            
    async def set_dns_static(self, name: str, address: str, ttl: int = 3600) -> bool:
        """Set static DNS entry."""
        try:
            # Check if entry exists
            entries = await self.get_dns_static()
            for entry in entries:
                if entry.get("name", "") == name:
                    # Update existing entry
                    entry_id = entry.get(".id", "")
                    if entry_id:
                        result = await self._call("PATCH", f"{REST_DNS_STATIC}/{entry_id}", {
                            ".id": entry_id,
                            "address": address,
                            "ttl": str(ttl)
                        })
                        return result is not None
                        
            # Create new entry
            result = await self._call("PUT", REST_DNS_STATIC, {
                "name": name,
                "address": address,
                "ttl": str(ttl)
            })
            return result is not None
        except Exception as e:
            log.warning("Failed to set static DNS entry: %s", e)
            return False
            
    async def remove_dns_static(self, name: str) -> bool:
        """Remove static DNS entry by name."""
        try:
            entries = await self.get_dns_static()
            for entry in entries:
                if entry.get("name", "") == name:
                    entry_id = entry.get(".id", "")
                    if entry_id:
                        result = await self._call("DELETE", f"{REST_DNS_STATIC}/{entry_id}", {".id": entry_id})
                        return result is not None
            return False
        except Exception as e:
            log.warning("Failed to remove static DNS entry: %s", e)
            return False
            
    async def get_routes(self) -> List[Dict]:
        """Get all routes."""
        try:
            return await self._call("GET", REST_ROUTE) or []
        except Exception as e:
            log.warning("Failed to get routes: %s", e)
            return []
            
    async def add_route(self, dst: str, gateway: str, comment: str = "") -> bool:
        """Add static route."""
        try:
            data = {
                "dst-address": dst,
                "gateway": gateway
            }
            if comment:
                data["comment"] = comment
                
            result = await self._call("PUT", REST_ROUTE, data)
            return result is not None
        except Exception as e:
            log.warning("Failed to add route: %s", e)
            return False
            
    async def get_system_info(self) -> Dict:
        """Get system information."""
        try:
            identity = await self._call("GET", REST_SYSTEM_IDENTITY) or {}
            resource = await self._call("GET", REST_SYSTEM_RESOURCE) or [{}]
            
            info = {
                "name": identity.get("name", ""),
                "version": resource[0].get("version", ""),
                "platform": resource[0].get("board-name", ""),
                "uptime": resource[0].get("uptime", ""),
            }
            
            # Try to get more detailed resource info
            try:
                cpu_raw = resource[0].get("cpu-load", "0")
                cpu_load = float(cpu_raw) if cpu_raw else 0.0
                info["cpu_load"] = cpu_load
            except:
                info["cpu_load"] = 0.0
                
            try:
                memory_raw = resource[0].get("free-memory", "0")
                free_memory = int(memory_raw) if memory_raw else 0
                info["free_memory"] = free_memory
            except:
                info["free_memory"] = 0
                
            return info
        except Exception as e:
            log.warning("Failed to get system info: %s", e)
            return {}
