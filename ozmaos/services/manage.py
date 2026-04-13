#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
# OzmaOS — Service management utility
#
# Shared utility for starting, stopping, and checking status of optional services.
# Used by both the CLI and the first-boot wizard.
#
# Usage:
#   python manage.py start <service>
#   python manage.py stop <service>
#   python manage.py status <service>
#   python manage.py list

from __future__ import annotations

import os
import subprocess
import sys
import json
from pathlib import Path
from typing import Optional


# Service directory relative to this script
SERVICES_DIR = Path(__file__).parent
BASE_DIR = SERVICES_DIR.parent.parent


def detect_hardware() -> dict[str, bool]:
    """Auto-detect available hardware for compose override generation."""
    hardware = {
        "nvidia_gpu": False,
        "amd_gpu": False,
        "intel_gpu": False,
        "hailo": False,
    }

    # NVIDIA GPU
    try:
        result = subprocess.run(
            ["nvidia-smi", "-L"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        hardware["nvidia_gpu"] = result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    # AMD GPU (ROCm)
    try:
        if Path("/dev/kfd").exists():
            hardware["amd_gpu"] = True
    except Exception:
        pass

    # Intel GPU
    try:
        if Path("/dev/dri").exists():
            for dev in Path("/dev/dri").iterdir():
                if dev.name.startswith("render"):
                    hardware["intel_gpu"] = True
                    break
    except Exception:
        pass

    # Hailo-8L (Raspberry Pi AI module)
    try:
        if Path("/dev/hailo0").exists():
            hardware["hailo"] = True
    except Exception:
        pass

    return hardware


def get_compose_env(service: str, hardware: dict[str, bool]) -> dict[str, str]:
    """Generate environment variables for compose file overrides."""
    env = {}

    if service == "frigate":
        if hardware["hailo"]:
            env["FRIGATE_HAILO_ENABLED"] = "true"
        elif hardware["nvidia_gpu"]:
            env["FRIGATE_NVIDIA_GPU"] = "true"

    elif service == "jellyfin":
        if hardware["nvidia_gpu"]:
            env["JELLYFIN_HARDWARE_ACCELERATION"] = "nvidia"
        elif hardware["amd_gpu"] or hardware["intel_gpu"]:
            env["JELLYFIN_HARDWARE_ACCELERATION"] = "vaapi"

    elif service == "immich":
        if hardware["nvidia_gpu"]:
            env["IMMICH_NVIDIA_GPU"] = "true"

    elif service == "ollama":
        if hardware["nvidia_gpu"]:
            env["OLLAMA_GPU"] = "nvidia"
        elif hardware["amd_gpu"]:
            env["OLLAMA_GPU"] = "amd"

    return env


def get_service_dir(service: str) -> Path:
    """Get the directory for a service."""
    service_dir = SERVICES_DIR / service
    if not service_dir.exists():
        raise ValueError(f"Service '{service}' not found. Available services: {list_services()}")
    return service_dir


def list_services() -> list[str]:
    """List all available services."""
    services = []
    for item in SERVICES_DIR.iterdir():
        if item.is_dir() and not item.name.startswith("_"):
            compose_file = item / "docker-compose.yml"
            if compose_file.exists():
                services.append(item.name)
    return sorted(services)


def run_compose(
    service: str,
    command: list[str],
    env_overrides: Optional[dict[str, str]] = None,
) -> int:
    """Run docker compose with the given command for a service."""
    service_dir = get_service_dir(service)

    # Base environment
    env = os.environ.copy()

    # Load .env file if present
    env_file = service_dir / "env.template"
    if env_file.exists():
        # Copy template to .env if it doesn't exist
        dotenv = service_dir / ".env"
        if not dotenv.exists():
            import shutil
            shutil.copy(env_file, dotenv)

    # Load existing .env
    dotenv = service_dir / ".env"
    if dotenv.exists():
        with open(dotenv) as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#") and "=" in line:
                    key, _, value = line.partition("=")
                    env[key.strip()] = value.strip()

    # Apply hardware-based overrides
    if env_overrides:
        env.update(env_overrides)

    compose_file = service_dir / "docker-compose.yml"

    cmd = [
        "docker", "compose",
        "-f", str(compose_file),
        *command,
    ]

    result = subprocess.run(cmd, cwd=service_dir, env=env)
    return result.returncode


def start(service: str) -> int:
    """Start a service."""
    hardware = detect_hardware()
    env_overrides = get_compose_env(service, hardware)

    print(f"Starting {service}...")
    print(f"  Hardware detected: {json.dumps(hardware, indent=2)}")

    # Pre-pull models for Ollama
    if service == "ollama":
        print("  Pre-pulling llama3.2 model...")
        subprocess.run(
            ["docker", "exec", "ozma-ollama", "ollama", "pull", "llama3.2"],
            check=False,  # Don't fail if model already exists
        )

    return run_compose(service, ["up", "-d"], env_overrides)


def stop(service: str) -> int:
    """Stop a service."""
    print(f"Stopping {service}...")
    return run_compose(service, ["down"])


def status(service: str) -> int:
    """Check status of a service."""
    # Get health check command from README
    service_dir = get_service_dir(service)
    readme = service_dir / "README.md"
    health_check = None

    if readme.exists():
        for line in readme.read_text().splitlines():
            if line.startswith("**Status check:**"):
                # Extract the command
                cmd_part = line.split("**Status check:**")[1].strip()
                health_check = cmd_part.strip("`")
                break

    # Check if containers are running
    result = run_compose(service, ["ps", "--format", "json"])

    if result == 0:
        # Parse container status from docker compose ps
        ps_result = subprocess.run(
            ["docker", "compose", "-f", str(service_dir / "docker-compose.yml"), "ps", "--format", "json"],
            cwd=service_dir,
            capture_output=True,
            text=True,
        )

        if ps_result.returncode == 0 and ps_result.stdout.strip():
            try:
                containers = [json.loads(line) for line in ps_result.stdout.strip().splitlines()]
                all_running = all(c.get("State") == "running" for c in containers)

                if all_running and health_check:
                    # Try health check
                    health_result = subprocess.run(
                        health_check,
                        shell=True,
                        capture_output=True,
                    )
                    if health_result.returncode == 0:
                        print(f"✓ {service}: healthy")
                        return 0
                    else:
                        print(f"✓ {service}: containers running, not yet healthy")
                        return 0
                elif all_running:
                    print(f"✓ {service}: running")
                    return 0
                else:
                    print(f"✗ {service}: containers not fully running")
                    return 1
            except json.JSONDecodeError:
                pass

    print(f"✗ {service}: not running")
    return 1


def restart(service: str) -> int:
    """Restart a service."""
    print(f"Restarting {service}...")
    stop_code = stop(service)
    if stop_code != 0:
        print("Warning: stop returned non-zero")
    return start(service)


def logs(service: str, follow: bool = False) -> int:
    """Show logs for a service."""
    service_dir = get_service_dir(service)
    cmd = ["docker", "compose", "-f", str(service_dir / "docker-compose.yml"), "logs"]
    if follow:
        cmd.append("-f")
    return subprocess.run(cmd, cwd=service_dir).returncode


def main() -> int:
    if len(sys.argv) < 2:
        print("OzmaOS Service Manager")
        print()
        print("Usage: python manage.py <command> [service]")
        print()
        print("Commands:")
        print("  list     List all available services")
        print("  start    Start a service")
        print("  stop     Stop a service")
        print("  restart  Restart a service")
        print("  status   Check service health")
        print("  logs     Show service logs")
        print()
        print("Services:")
        for svc in list_services():
            print(f"  - {svc}")
        return 0

    command = sys.argv[1]

    if command == "list":
        for svc in list_services():
            print(svc)
        return 0

    if command in ("start", "stop", "restart", "status", "logs"):
        if len(sys.argv) < 3:
            print(f"Error: {command} requires a service name")
            print(f"Available services: {', '.join(list_services())}")
            return 1

        service = sys.argv[2]

        # Handle log follow flag
        if command == "logs" and len(sys.argv) > 3 and sys.argv[3] == "-f":
            return logs(service, follow=True)

        # Dispatch command
        commands = {
            "start": start,
            "stop": stop,
            "restart": restart,
            "status": status,
            "logs": logs,
        }

        return commands[command](service)

    print(f"Unknown command: {command}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
