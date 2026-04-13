#!/usr/bin/env python3
"""
Service Definitions for OzmaOS First-Boot Wizard

Maps service names to Docker Compose files and systemd units.
"""

from typing import Any


SERVICES = {
    "controller": {
        "display_name": "Ozma Controller",
        "description": "Main system controller and web interface",
        "docker_compose": None,
        "systemd": ["ozma-controller.service"],
        "default_enabled": True,
        "port": 7380,
    },
    "frigate": {
        "display_name": "Frigate NVR",
        "description": "Camera network video recorder with AI detection",
        "docker_compose": "/opt/ozmaos/compose/frigate/docker-compose.yml",
        "systemd": [],
        "default_enabled": True,
        "requires_gpu": True,
        "port": 5000,
    },
    "vaultwarden": {
        "display_name": "Vaultwarden",
        "description": "Self-hosted password manager (Bitwarden-compatible)",
        "docker_compose": "/opt/ozmaos/compose/vaultwarden/docker-compose.yml",
        "systemd": [],
        "default_enabled": True,
        "port": 7080,
    },
    "jellyfin": {
        "display_name": "Jellyfin",
        "description": "Media server for movies, TV, music, and photos",
        "docker_compose": "/opt/ozmaos/compose/jellyfin/docker-compose.yml",
        "systemd": [],
        "default_enabled": False,
        "requires_gpu": True,
        "port": 8096,
    },
    "immich": {
        "display_name": "Immich",
        "description": "Self-hosted photo and video backup solution",
        "docker_compose": "/opt/ozmaos/compose/immich/docker-compose.yml",
        "systemd": [],
        "default_enabled": False,
        "port": 2283,
    },
    "authentik": {
        "display_name": "Authentik",
        "description": "Self-hosted identity provider (SSO, OAuth, SAML)",
        "docker_compose": "/opt/ozmaos/compose/authentik/docker-compose.yml",
        "systemd": [],
        "default_enabled": False,
        "port": 9000,
    },
    "ollama": {
        "display_name": "Ollama",
        "description": "Local AI model server (Llama, Mistral, etc.)",
        "docker_compose": "/opt/ozmaos/compose/ollama/docker-compose.yml",
        "systemd": [],
        "default_enabled": False,
        "requires_hailo": False,
        "port": 11434,
    },
}


def get_available_services(hardware: dict[str, Any]) -> list[str]:
    """Filter services based on available hardware."""
    available = []
    
    for name, info in SERVICES.items():
        # Check GPU requirement
        if info.get("requires_gpu"):
            gpu_count = hardware.get("gpu_count", 0)
            if gpu_count == 0:
                continue
        
        # Check Hailo requirement
        if info.get("requires_hailo"):
            if not hardware.get("hailo", {}).get("available"):
                continue
        
        # Check if compose file exists
        compose_file = info.get("docker_compose")
        if compose_file is not None:
            from pathlib import Path
            if not Path(compose_file).exists():
                continue
        
        available.append(name)
    
    return available


def get_service_info(name: str) -> dict[str, Any]:
    """Get information about a service."""
    return SERVICES.get(name, {})


def get_default_services() -> list[str]:
    """Get list of services that should be enabled by default."""
    return [name for name, info in SERVICES.items() if info.get("default_enabled", False)]


def get_all_services() -> list[str]:
    """Get list of all available services."""
    return list(SERVICES.keys())


if __name__ == "__main__":
    import json
    import detect_hardware
    
    hw = detect_hardware.detect()
    available = get_available_services(hw)
    
    print(json.dumps({
        "all_services": get_all_services(),
        "default_services": get_default_services(),
        "available_services": available,
    }, indent=2))
