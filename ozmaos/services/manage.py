#!/usr/bin/env python3
"""
OzmaOS Service Manager

Unified management interface for OzmaOS optional services.
Provides start/stop/status commands for each Docker Compose stack.

Usage:
    python manage.py start <service>
    python manage.py stop <service>
    python manage.py status <service>
    python manage.py list
    python manage.py logs <service>
    python manage.py gpu-detect
"""

import argparse
import subprocess
import sys
import json
from pathlib import Path

# Service definitions
SERVICES = {
    "frigate": {"port": 5000, "has_gpu": True},
    "vaultwarden": {"port": 8222, "has_gpu": False},
    "authentik": {"port": 9000, "has_gpu": False},
    "jellyfin": {"port": 8096, "has_gpu": True},
    "immich": {"port": 2283, "has_gpu": True},
    "audiobookshelf": {"port": 13378, "has_gpu": False},
    "ollama": {"port": 11434, "has_gpu": True},
}

SERVICES_DIR = Path(__file__).parent


def run_command(cmd: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess:
    """Run a command and return the result."""
    try:
        return subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        print(f"Error running command: {' '.join(cmd)}")
        print(e.stderr)
        sys.exit(1)


def detect_nvidia_gpu() -> bool:
    """Check if NVIDIA GPU is available."""
    try:
        result = subprocess.run(
            ["nvidia-smi"],
            capture_output=True,
        )
        return result.returncode == 0
    except FileNotFoundError:
        return False


def detect_hailo() -> bool:
    """Check if Hailo-8L is available."""
    try:
        result = subprocess.run(
            ["lsusb", "-d", "1e60:"],
            capture_output=True,
            text=True,
        )
        return "hailo" in result.stdout.lower() or result.returncode == 0
    except FileNotFoundError:
        return False


def detect_amd_gpu() -> bool:
    """Check if AMD GPU is available for ROCm."""
    try:
        result = subprocess.run(
            ["ls", "/dev/kfd"],
            capture_output=True,
        )
        return result.returncode == 0
    except FileNotFoundError:
        return False


def detect_gpu() -> dict:
    """Auto-detect available GPU hardware."""
    return {
        "nvidia": detect_nvidia_gpu(),
        "hailo": detect_hailo(),
        "amd": detect_amd_gpu(),
    }


def start_service(service_name: str, gpu_info: dict | None = None) -> None:
    """Start a service."""
    service_dir = SERVICES_DIR / service_name
    
    if not service_dir.exists():
        print(f"Service '{service_name}' not found")
        print(f"Available services: {', '.join(SERVICES.keys())}")
        sys.exit(1)
    
    compose_file = service_dir / "docker-compose.yml"
    if not compose_file.exists():
        print(f"Error: docker-compose.yml not found in {service_dir}")
        sys.exit(1)
    
    # Check for .env file
    env_file = service_dir / ".env"
    if not env_file.exists():
        env_template = service_dir / "env.template"
        if env_template.exists():
            print(f"Warning: No .env file found. Copy env.template to .env")
    
    # Build environment with GPU detection
    env = {
        **gpu_info,
        "DOCKER_NVIDIA_VISIBLE_DEVICES": "all" if gpu_info.get("nvidia") else "",
    }
    
    # Filter empty values
    env = {k: v for k, v in env.items() if v}
    
    print(f"Starting {service_name}...")
    
    cmd = ["docker", "compose", "up", "-d"]
    
    # Set environment variables for docker compose
    for key, value in env.items():
        if value:
            run_command(
                ["docker", "compose", "up", "-d", "--env-file", str(env_file)] if env_file.exists() else cmd,
                cwd=service_dir,
            )
            break
    else:
        run_command(cmd, cwd=service_dir)
    
    print(f"{service_name} started")
    print(f"Check status with: python manage.py status {service_name}")


def stop_service(service_name: str) -> None:
    """Stop a service."""
    service_dir = SERVICES_DIR / service_name
    
    if not service_dir.exists():
        print(f"Service '{service_name}' not found")
        sys.exit(1)
    
    print(f"Stopping {service_name}...")
    run_command(["docker", "compose", "down"], cwd=service_dir)
    print(f"{service_name} stopped")


def status_service(service_name: str) -> None:
    """Show status of a service."""
    service_dir = SERVICES_DIR / service_name
    
    if not service_dir.exists():
        print(f"Service '{service_name}' not found")
        sys.exit(1)
    
    result = subprocess.run(
        ["docker", "compose", "ps"],
        cwd=service_dir,
        capture_output=True,
        text=True,
    )
    
    if result.stdout.strip():
        print(f"\n=== {service_name} ===")
        print(result.stdout)
    else:
        print(f"\n{ service_name}: not running")


def logs_service(service_name: str, follow: bool = False) -> None:
    """Show logs for a service."""
    service_dir = SERVICES_DIR / service_name
    
    if not service_dir.exists():
        print(f"Service '{service_name}' not found")
        sys.exit(1)
    
    cmd = ["docker", "compose", "logs"]
    if follow:
        cmd.append("-f")
    
    run_command(cmd, cwd=service_dir)


def list_services(gpu_info: dict | None = None) -> None:
    """List all available services."""
    print("\n=== OzmaOS Services ===\n")
    
    for name, info in SERVICES.items():
        status = subprocess.run(
            ["docker", "compose", "ps", "-q"],
            cwd=SERVICES_DIR / name,
            capture_output=True,
        )
        
        running = bool(status.stdout.strip())
        port = info["port"]
        has_gpu = info["has_gpu"]
        
        status_icon = "●" if running else "○"
        gpu_icon = " [GPU]" if has_gpu and gpu_info and gpu_info.get("nvidia") else ""
        
        print(f"  {status_icon} {name:20} port {port}{gpu_icon}")
    
    print()


def main():
    parser = argparse.ArgumentParser(
        description="OzmaOS Service Manager",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Services:
  frigate          NVR with object detection (Hailo-8L/GPU)
  vaultwarden      Password manager (Bitwarden-compatible)
  authentik        Identity provider (OIDC/SAML)
  jellyfin         Media server
  immich           Photo/video backup
  audiobookshelf   Audiobook/podcast server
  ollama           Local LLM inference

Examples:
  python manage.py list
  python manage.py start frigate
  python manage.py status vaultwarden
  python manage.py logs -f immich
  python manage.py gpu-detect
        """,
    )
    
    subparsers = parser.add_subparsers(dest="command", required=True)
    
    # list command
    subparsers.add_parser("list", help="List all services")
    
    # gpu-detect command
    subparsers.add_parser("gpu-detect", help="Detect available GPU hardware")
    
    # start command
    start_parser = subparsers.add_parser("start", help="Start a service")
    start_parser.add_argument("service", choices=list(SERVICES.keys()))
    
    # stop command
    stop_parser = subparsers.add_parser("stop", help="Stop a service")
    stop_parser.add_argument("service", choices=list(SERVICES.keys()))
    
    # status command
    status_parser = subparsers.add_parser("status", help="Show service status")
    status_parser.add_argument("service", choices=list(SERVICES.keys()))
    
    # logs command
    logs_parser = subparsers.add_parser("logs", help="Show service logs")
    logs_parser.add_argument("service", choices=list(SERVICES.keys()))
    logs_parser.add_argument("-f", "--follow", action="store_true", help="Follow logs")
    
    args = parser.parse_args()
    
    # Get GPU info for commands that need it
    gpu_info = detect_gpu() if args.command in ("list", "start") else None
    
    if args.command == "list":
        list_services(gpu_info)
    elif args.command == "gpu-detect":
        info = detect_gpu()
        print("\n=== GPU Detection ===\n")
        print(f"  NVIDIA GPU:  {'✓' if info['nvidia'] else '✗'}")
        print(f"  Hailo-8L:    {'✓' if info['hailo'] else '✗'}")
        print(f"  AMD GPU:     {'✓' if info['amd'] else '✗'}")
        print()
    elif args.command == "start":
        start_service(args.service, gpu_info)
    elif args.command == "stop":
        stop_service(args.service)
    elif args.command == "status":
        status_service(args.service)
    elif args.command == "logs":
        logs_service(args.service, args.follow)


if __name__ == "__main__":
    main()
