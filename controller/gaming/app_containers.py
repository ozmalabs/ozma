# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Container-based game isolation.

Provides Docker/Podman game isolation with per-app persistent home directories
and fake-udev for gamepad hotplug.

Features:
  - Docker/Podman game isolation
  - Per-app persistent home dir (auto-mount)
  - Fake-udev for gamepad hotplug in containers
  - GPU passthrough to container (render node)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.app_containers")


# ─── Constants ───────────────────────────────────────────────────────────────

CONTAINER_PREFIX = "ozma-game-"
# Default GPU device - can be overridden in ContainerConfig
# Common values: /dev/dri/renderD128 (Intel), /dev/dri/renderD129 (NVIDIA), /dev/dri/card0 (generic)
DEFAULT_GPU_DEVICE = "/dev/dri/renderD128"
DEFAULT_PODMAN_RUNTIME = "runc"


class GPUError(Exception):
    """Raised when no GPU device is available."""
    pass


def get_gpu_device() -> str:
    """
    Detect available GPU device automatically.

    Returns:
        Path to detected GPU device.

    Raises:
        GPUError: If no GPU device is available.
    """
    import os

    # Try Intel render nodes first (most common)
    for i in range(128, 144):
        device = f"/dev/dri/renderD{i}"
        if os.path.exists(device):
            log.info("Detected GPU device: %s", device)
            return device

    # Try NVIDIA render nodes
    for i in range(128, 144):
        device = f"/dev/dri/renderD{i}"
        if os.path.exists(device):
            log.info("Detected GPU device: %s", device)
            return device

    # Fallback to generic card0
    device = "/dev/dri/card0"
    if os.path.exists(device):
        log.info("Detected GPU device: %s", device)
        return device

    raise GPUError(
        "No GPU device found. Please ensure a GPU is available at /dev/dri/renderD* or /dev/dri/card*"
    )


# ─── Container Configuration ─────────────────────────────────────────────────

import re


@dataclass
class ContainerConfig:
    """Configuration for a game container."""
    app_id: str
    image: str = "alpine:latest"
    command: list[str] = field(default_factory=lambda: ["/bin/sh"])
    working_dir: str = "/home/user"
    volumes: list[dict[str, str]] = field(default_factory=list)
    env: dict[str, str] = field(default_factory=dict)
    gpu: bool = True
    gpu_device: str = DEFAULT_GPU_DEVICE
    network: str = "bridge"
    memory_limit: str = "4G"
    cpu_limit: str = "2"
    device_cgroup_rules: list[str] = field(default_factory=list)
    auto_start: bool = False
    auto_remove: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "app_id": self.app_id,
            "image": self.image,
            "command": self.command,
            "working_dir": self.working_dir,
            "volumes": self.volumes,
            "env": self.env,
            "gpu": self.gpu,
            "gpu_device": self.gpu_device,
            "network": self.network,
            "memory_limit": self.memory_limit,
            "cpu_limit": self.cpu_limit,
            "device_cgroup_rules": self.device_cgroup_rules,
            "auto_start": self.auto_start,
            "auto_remove": self.auto_remove,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "ContainerConfig":
        """Create a ContainerConfig from a dictionary, handling all fields."""
        return cls(
            app_id=data.get("app_id", ""),
            image=data.get("image", "alpine:latest"),
            command=data.get("command", ["/bin/sh"]),
            working_dir=data.get("working_dir", "/home/user"),
            volumes=data.get("volumes", []),
            env=data.get("env", {}),
            gpu=data.get("gpu", True),
            gpu_device=data.get("gpu_device", DEFAULT_GPU_DEVICE),
            network=data.get("network", "bridge"),
            memory_limit=data.get("memory_limit", "4G"),
            cpu_limit=data.get("cpu_limit", "2"),
            device_cgroup_rules=data.get("device_cgroup_rules", []),
            auto_start=data.get("auto_start", False),
            auto_remove=data.get("auto_remove", True),
        )

    @staticmethod
    def validate_app_id(app_id: str) -> bool:
        """
        Validate app_id format.

        Args:
            app_id: The application identifier to validate.

        Returns:
            True if valid, False otherwise.

        Validates:
            - Length (1-64 characters)
            - Allowed characters (alphanumeric, hyphen, underscore, dot)
            - No consecutive hyphens or underscores
            - Does not start or end with hyphen or underscore
        """
        if not app_id or len(app_id) > 64:
            return False

        # Check for valid characters
        if not re.match(r"^[a-zA-Z0-9][a-zA-Z0-9._-]*[a-zA-Z0-9]$", app_id):
            # Allow single-character IDs
            if len(app_id) == 1 and not re.match(r"^[a-zA-Z0-9]$", app_id):
                return False

        # Check for consecutive hyphens or underscores
        if "--" in app_id or "__" in app_id:
            return False

        # Check for consecutive dots
        if ".." in app_id:
            return False

        return True

    def validate(self) -> tuple[bool, list[str]]:
        """
        Validate the container configuration.

        Returns:
            Tuple of (is_valid, list_of_error_messages).
        """
        errors = []

        # Validate app_id
        if not self.app_id:
            errors.append("app_id is required")
        elif not self.validate_app_id(self.app_id):
            errors.append(
                f"Invalid app_id '{self.app_id}': "
                "must be 1-64 chars, alphanumeric/hyphen/underscore/dot only"
            )

        # Validate image
        if not self.image or not self.image.strip():
            errors.append("image is required")

        # Validate network
        valid_networks = ["bridge", "host", "none", "container"]
        if self.network not in valid_networks:
            errors.append(
                f"Invalid network '{self.network}': "
                f"must be one of {valid_networks}"
            )

        # Validate GPU device if GPU is enabled
        if self.gpu and self.gpu_device and not Path(self.gpu_device).exists():
            log.warning(
                "GPU device %s specified but not found - may fail at runtime",
                self.gpu_device
            )

        # Validate CPU limit
        try:
            float(self.cpu_limit)
        except (ValueError, TypeError):
            errors.append(f"Invalid cpu_limit '{self.cpu_limit}': must be a number")

        # Validate memory limit format (e.g., "4G", "512M", "2048")
        if not re.match(r"^\d+([KMGT]?)$", self.memory_limit.upper()):
            errors.append(
                f"Invalid memory_limit '{self.memory_limit}': "
                "must be in format like '4G', '512M', '2048'"
            )

        return (len(errors) == 0, errors)

    def generate_container_name(self, session_id: str | None = None) -> str:
        """
        Generate a unique container name based on configuration.

        Args:
            session_id: Optional session identifier for uniqueness.

        Returns:
            A valid container name.
        """
        # Sanitize app_id for container name
        safe_app_id = re.sub(r"[^a-zA-Z0-9]", "-", self.app_id.lower())

        if session_id:
            # Include session ID for per-session containers
            return f"{CONTAINER_PREFIX}{safe_app_id[:16]}-{session_id[:8]}"
        else:
            # Use hash for unique naming
            import hashlib
            hash_suffix = hashlib.md5(self.app_id.encode()).hexdigest()[:8]
            return f"{CONTAINER_PREFIX}{safe_app_id[:20]}-{hash_suffix}"


@dataclass
class ContainerInfo:
    """Information about a running container."""
    container_id: str
    app_id: str
    pid: int | None = None
    started_at: float = field(default_factory=time.time)
    exited_at: float | None = None
    exit_code: int | None = None
    logs: list[str] = field(default_factory=list)
    state: str = "created"


# ─── Container Manager ───────────────────────────────────────────────────────

class ContainerManager:
    """
    Manages game containers with proper isolation.

    Features:
      - Docker/Podman container lifecycle
      - Per-app persistent home directories
      - GPU passthrough
      - Fake-udev for gamepad hotplug
    """

    def __init__(
        self,
        data_dir: Path = Path("/var/lib/ozma/gaming/containers"),
    ):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._home_dir = self._data_dir / "homes"
        self._home_dir.mkdir(parents=True, exist_ok=True)

        # Container state
        self._configs: dict[str, ContainerConfig] = {}
        self._containers: dict[str, ContainerInfo] = {}
        self._home_mounts: dict[str, Path] = {}

        # Load persisted state
        self._load_state()

    def _load_state(self) -> None:
        """Load container configurations from disk."""
        config_file = self._data_dir / "configs.json"
        if config_file.exists():
            try:
                data = json.loads(config_file.read_text())
                for app_id, cfg_data in data.get("configs", {}).items():
                    self._configs[app_id] = ContainerConfig.from_dict(cfg_data)
                log.info("Loaded %d container configurations", len(self._configs))
            except Exception as e:
                log.error("Failed to load container configs: %s", e)

    def _save_state(self) -> None:
        """Save container configurations to disk."""
        config_file = self._data_dir / "configs.json"
        try:
            data = {
                "configs": {
                    app_id: cfg.to_dict()
                    for app_id, cfg in self._configs.items()
                },
                "last_save": time.time(),
            }
            config_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save container configs: %s", e)

    def setup_app_home(self, app_id: str, username: str = "user") -> Path:
        """Create and return the persistent home directory for an app."""
        home_path = self._home_dir / app_id / username
        home_path.mkdir(parents=True, exist_ok=True)

        # Copy default config files if they don't exist
        default_home = Path(__file__).parent / "default_home"
        if default_home.exists():
            for item in default_home.iterdir():
                dest = home_path / item.name
                if not dest.exists():
                    if item.is_dir():
                        shutil.copytree(item, dest, symlinks=True)
                    else:
                        shutil.copy2(item, dest)
        else:
            # Create basic structure if no default_home
            (home_path / ".config").mkdir(parents=True, exist_ok=True)
            (home_path / "saves").mkdir(parents=True, exist_ok=True)

        self._home_mounts[app_id] = home_path
        return home_path

    def create_config(
        self,
        app_id: str,
        image: str = "alpine:latest",
        command: list[str] | None = None,
        **kwargs,
    ) -> ContainerConfig:
        """Create a container configuration for an app."""
        config = ContainerConfig(
            app_id=app_id,
            image=image,
            command=command or ["/bin/sh"],
            **kwargs,
        )
        self._configs[app_id] = config
        self._save_state()
        return config

    def get_config(self, app_id: str) -> ContainerConfig | None:
        """Get the configuration for an app."""
        return self._configs.get(app_id)

    def get_or_create_home(self, app_id: str) -> Path:
        """Get or create the home directory for an app."""
        return self._home_mounts.get(app_id) or self.setup_app_home(app_id)

    async def start_container(
        self,
        app_id: str,
        container_name: str | None = None,
        extra_volumes: list[dict[str, str]] | None = None,
    ) -> ContainerInfo | None:
        """Start a game container."""
        config = self._configs.get(app_id)
        if not config:
            log.error("No configuration for app %s", app_id)
            return None

        container_name = container_name or f"{CONTAINER_PREFIX}{app_id[:8]}"

        # Build command
        cmd = self._build_podman_command(config, container_name, extra_volumes)

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )

            # Wait for container to start
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30.0)

            if proc.returncode == 0:
                container_id = stdout.decode().strip()
                info = ContainerInfo(
                    container_id=container_id,
                    app_id=app_id,
                    pid=proc.pid,
                    started_at=time.time(),
                )
                self._containers[container_id] = info
                return info
            else:
                log.error("Failed to start container: %s", stderr.decode())
                return None
        except Exception as e:
            log.error("Failed to start container: %s", e)
            return None

    async def stop_container(self, container_id: str) -> bool:
        """Stop a container."""
        info = self._containers.get(container_id)
        if not info:
            return False

        try:
            result = await asyncio.create_subprocess_exec(
                "podman", "stop", container_id,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await result.communicate()

            if result.returncode == 0:
                info.exited_at = time.time()
                info.exit_code = result.returncode
                del self._containers[container_id]
                return True
            else:
                log.error("Failed to stop container: %s", stderr.decode())
                return False
        except Exception as e:
            log.error("Failed to stop container: %s", e)
            return False

    def _build_podman_command(
        self,
        config: ContainerConfig,
        container_name: str,
        extra_volumes: list[dict[str, str]] | None = None,
    ) -> list[str]:
        """Build the podman run command."""
        cmd = ["podman", "run", "--rm", "--name", container_name]

        # Memory limit
        cmd.extend(["--memory", config.memory_limit])

        # CPU limit
        cmd.extend(["--cpus", config.cpu_limit])

        # Network
        cmd.extend(["--network", config.network])

        # GPU passthrough
        if config.gpu:
            cmd.extend(["--device", config.gpu_device])
            cmd.extend([
                "--device-cgroup-rule", "c 199:* rmw",
                "--device-cgroup-rule", "c 226:* rmw",
            ])

        # Home directory bind mount
        home_path = self.get_or_create_home(config.app_id)
        cmd.extend([
            "-v", f"{home_path}:/home/user:Z",
        ])

        # Additional volumes
        for vol in (config.volumes + (extra_volumes or [])):
            cmd.extend(["-v", f"{vol['host']}:{vol['container']}:{vol.get('opts', 'Z')}"])

        # Environment variables
        for key, value in config.env.items():
            cmd.extend(["-e", f"{key}={value}"])

        # Working directory
        cmd.extend(["-w", config.working_dir])

        # Image
        cmd.append(config.image)

        # Command
        cmd.extend(config.command)

        return cmd

    def get_container(self, container_id: str) -> ContainerInfo | None:
        """Get container information."""
        return self._containers.get(container_id)

    def get_containers_for_app(self, app_id: str) -> list[ContainerInfo]:
        """Get all containers for an app."""
        return [c for c in self._containers.values() if c.app_id == app_id]

    async def start_all_auto_start(self) -> None:
        """Start all containers with auto_start enabled."""
        for app_id, config in self._configs.items():
            if config.auto_start:
                await self.start_container(app_id)

    async def stop_all(self) -> None:
        """Stop all containers."""
        for container_id in list(self._containers.keys()):
            await self.stop_container(container_id)


# ─── Fake Udev Manager ───────────────────────────────────────────────────────

class FakeUdevManager:
    """
    Simulates udev events for gamepad hotplug in containers.

    Provides fake-udev pattern for gamepad detection without real hardware.
    """

    def __init__(self, data_dir: Path = Path("/run/udev")):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._devices: dict[str, dict[str, Any]] = {}

    def add_device(self, name: str, device_type: str = "gamepad") -> str:
        """Add a fake udev device."""
        device_id = f"ozma_{name}_{time.time_ns() % 10000:04d}"

        self._devices[device_id] = {
            "name": name,
            "type": device_type,
            "devnode": f"/dev/input/{device_type}_{device_id}",
            "syspath": f"/sys/class/input/{device_type}_{device_id}",
            "properties": {
                "ID_VENDOR_ID": "0x1234",
                "ID_MODEL_ID": "0x5678",
                "ID_INPUT_GAMEPAD": "1",
                "ID_INPUT_GAMEPAD_KEY": "1",
            },
            "created_at": time.time(),
        }

        # Write device info file (simulated udev)
        device_file = self._data_dir / f"{device_id}.udev"
        device_file.write_text(json.dumps(self._devices[device_id]))

        log.info("Added fake udev device: %s", device_id)
        return device_id

    def remove_device(self, device_id: str) -> bool:
        """Remove a fake udev device."""
        if device_id not in self._devices:
            return False

        del self._devices[device_id]
        device_file = self._data_dir / f"{device_id}.udev"
        if device_file.exists():
            device_file.unlink()

        log.info("Removed fake udev device: %s", device_id)
        return True

    def get_device(self, device_id: str) -> dict[str, Any] | None:
        """Get a fake udev device."""
        return self._devices.get(device_id)

    def get_all_devices(self) -> list[dict[str, Any]]:
        """Get all fake udev devices."""
        return list(self._devices.values())

    def simulate_hotplug(self, device_id: str, connected: bool) -> None:
        """Simulate a udev hotplug event."""
        device = self._devices.get(device_id)
        if not device:
            return

        event = {
            "action": "add" if connected else "remove",
            "device": device,
            "timestamp": time.time(),
        }

        log.info("Simulated udev %s event for %s", event["action"], device_id)
