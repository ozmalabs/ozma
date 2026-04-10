# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for system health and status.
"""

import asyncio
import logging
import time
from datetime import datetime, timezone
from typing import TYPE_CHECKING

from strawberry import type as graphql_type
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.system")


@graphql_type
class NetworkInterface:
    """
    Represents a network interface and its status.
    
    Fields:
        name: Interface name (e.g., "eth0", "wlan0")
        ip_address: IPv4 address
        ipv6_address: IPv6 address (optional)
        mac_address: MAC address
        speed_mbps: Interface speed in Mbps
        status: "up" or "down"
        rx_bytes: Total received bytes
        tx_bytes: Total transmitted bytes
        rx_errors: Receive errors
        tx_errors: Transmit errors
    """
    name: str
    ip_address: str
    ipv6_address: str | None
    mac_address: str
    speed_mbps: int
    status: str
    rx_bytes: int
    tx_bytes: int
    rx_errors: int
    tx_errors: int


@graphql_type
class SystemHealth:
    """
    Represents overall system health status.
    
    Fields:
        uptime_seconds: How long the controller has been running
        start_time: Timestamp when the controller started
        active_node_id: ID of the currently active node
        active_scenario_id: ID of the currently active scenario
        node_count: Total number of registered nodes
        online_nodes: Number of nodes that recently heartbeat
        audio_enabled: Whether audio routing is enabled
        auth_enabled: Whether authentication is enabled
        mesh_connected: Whether mesh CA is connected
        vban_enabled: Whether VBAN audio is enabled
        network_interfaces: List of network interfaces with status
        last_heartbeat_at: Timestamp of last node heartbeat
        cpu_usage: CPU usage percentage (approximate)
        memory_usage: Memory usage percentage (approximate)
    """
    uptime_seconds: int
    start_time: float
    active_node_id: str | None
    active_scenario_id: str | None
    node_count: int
    online_nodes: int
    audio_enabled: bool
    auth_enabled: bool
    mesh_connected: bool
    vban_enabled: bool
    network_interfaces: list[NetworkInterface]
    last_heartbeat_at: float | None
    cpu_usage: float
    memory_usage: float


async def resolve_system_health(info: Info) -> SystemHealth:
    """
    Get overall system health status.
    
    Returns:
        SystemHealth: Current system health and status
    """
    state: AppState = info.graphql_context["state"]
    
    # Calculate uptime
    import os
    start_time = getattr(state, '_start_time', time.monotonic())
    uptime_seconds = int(time.monotonic() - start_time)
    
    # Get active node and scenario
    active_node_id = state.active_node_id
    active_scenario_id = None  # Would need to track this in ScenarioManager
    
    # Count nodes
    node_count = len(state.nodes)
    online_nodes = 0
    last_heartbeat_at = None
    
    # Check node online status based on last_seen timestamp
    for node in state.nodes.values():
        # Consider online if last_seen within last 60 seconds
        if time.monotonic() - node.last_seen < 60:
            online_nodes += 1
            if last_heartbeat_at is None or node.last_seen > last_heartbeat_at:
                last_heartbeat_at = node.last_seen
    
    # Audio status
    audio_enabled = False
    vban_enabled = False
    
    # Check if any nodes have audio configured
    for node in state.nodes.values():
        if node.audio_type:
            audio_enabled = True
            if node.audio_type == "vban":
                vban_enabled = True
                break
    
    # Auth status (get from state or config)
    auth_enabled = False
    auth_config = getattr(state, 'auth_config', None)
    if auth_config:
        auth_enabled = getattr(auth_config, 'enabled', False)
    
    # Mesh status
    mesh_connected = False
    mesh_ca = getattr(state, 'mesh_ca', None)
    if mesh_ca:
        # Check if we have a controller keypair
        mesh_connected = hasattr(mesh_ca, 'controller_keypair') and mesh_ca.controller_keypair is not None
    
    # Network interfaces (simplified - would need to parse /proc/net or use psutil)
    network_interfaces = _get_network_interfaces()
    
    # CPU and memory usage (use /proc/stat and /proc/meminfo)
    cpu_usage = _get_cpu_usage()
    memory_usage = _get_memory_usage()
    
    return SystemHealth(
        uptime_seconds=uptime_seconds,
        start_time=start_time,
        active_node_id=active_node_id,
        active_scenario_id=active_scenario_id,
        node_count=node_count,
        online_nodes=online_nodes,
        audio_enabled=audio_enabled,
        auth_enabled=auth_enabled,
        mesh_connected=mesh_connected,
        vban_enabled=vban_enabled,
        network_interfaces=network_interfaces,
        last_heartbeat_at=last_heartbeat_at,
        cpu_usage=cpu_usage,
        memory_usage=memory_usage,
    )


def _get_network_interfaces() -> list[NetworkInterface]:
    """
    Get network interface information.
    
    Returns a list of NetworkInterface objects with basic info.
    On Linux, this reads from /proc/net/dev and /sys/class/net/.
    """
    interfaces: list[NetworkInterface] = []
    
    try:
        # Read /proc/net/dev for interface stats
        with open('/proc/net/dev', 'r') as f:
            lines = f.readlines()
        
        # Parse header and data lines
        for line in lines[2:]:  # Skip first 2 header lines
            parts = line.strip().split(':')
            if len(parts) != 2:
                continue
            
            name = parts[0].strip()
            
            # Skip loopback interface
            if name == 'lo':
                continue
            
            stats = parts[1].split()
            if len(stats) < 16:
                continue
            
            # Parse statistics
            try:
                rx_bytes = int(stats[0])
                rx_errors = int(stats[2])
                tx_bytes = int(stats[8])
                tx_errors = int(stats[10])
            except (ValueError, IndexError):
                continue
            
            # Get interface info from /sys/class/net/
            ip_address = _get_interface_ip(name)
            ipv6_address = _get_interface_ipv6(name)
            mac_address = _get_interface_mac(name)
            speed_mbps = _get_interface_speed(name)
            status = _get_interface_status(name)
            
            interfaces.append(NetworkInterface(
                name=name,
                ip_address=ip_address,
                ipv6_address=ipv6_address,
                mac_address=mac_address,
                speed_mbps=speed_mbps,
                status=status,
                rx_bytes=rx_bytes,
                tx_bytes=tx_bytes,
                rx_errors=rx_errors,
                tx_errors=tx_errors,
            ))
    except Exception as e:
        log.debug("Failed to read network interfaces: %s", e)
    
    return interfaces


def _get_interface_ip(name: str) -> str:
    """Get IPv4 address for an interface."""
    try:
        import socket
        import fcntl
        import struct
        
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        ip = fcntl.ioctl(
            s.fileno(),
            0x8915,  # SIOCGIFADDR
            struct.pack('256s', name[:15].encode('utf-8'))
        )
        return socket.inet_ntoa(ip[20:24])
    except Exception:
        return "0.0.0.0"


def _get_interface_ipv6(name: str) -> str | None:
    """Get IPv6 address for an interface."""
    try:
        with open(f'/sys/class/net/{name}/address', 'r') as f:
            return f.read().strip()
    except Exception:
        return None


def _get_interface_mac(name: str) -> str:
    """Get MAC address for an interface."""
    try:
        with open(f'/sys/class/net/{name}/address', 'r') as f:
            return f.read().strip()
    except Exception:
        return "00:00:00:00:00:00"


def _get_interface_speed(name: str) -> int:
    """Get interface speed in Mbps."""
    try:
        with open(f'/sys/class/net/{name}/speed', 'r') as f:
            speed = int(f.read().strip())
            return speed if speed > 0 else 1000  # Default to 1Gbps if unknown
    except Exception:
        return 1000  # Default to 1Gbps


def _get_interface_status(name: str) -> str:
    """Get interface status (up/down)."""
    try:
        with open(f'/sys/class/net/{name}/operstate', 'r') as f:
            status = f.read().strip()
            return 'up' if status == 'up' else 'down'
    except Exception:
        return 'down'


def _get_cpu_usage() -> float:
    """Get approximate CPU usage percentage."""
    try:
        with open('/proc/stat', 'r') as f:
            line = f.readline()
        
        # Parse cpu line: cpu  user nice system idle iowait irq softirq steal guest guest_nice
        parts = line.split()
        if len(parts) < 5:
            return 0.0
        
        values = [int(x) for x in parts[1:5]]
        total = sum(values)
        idle = values[3]
        
        if total == 0:
            return 0.0
        
        return round((total - idle) / total * 100, 1)
    except Exception:
        return 0.0


def _get_memory_usage() -> float:
    """Get memory usage percentage."""
    try:
        with open('/proc/meminfo', 'r') as f:
            meminfo = f.read()
        
        mem_total = 0
        mem_available = 0
        
        for line in meminfo.split('\n'):
            if line.startswith('MemTotal:'):
                mem_total = int(line.split()[1])
            elif line.startswith('MemAvailable:'):
                mem_available = int(line.split()[1])
        
        if mem_total == 0:
            return 0.0
        
        mem_used = mem_total - mem_available
        return round(mem_used / mem_total * 100, 1)
    except Exception:
        return 0.0
