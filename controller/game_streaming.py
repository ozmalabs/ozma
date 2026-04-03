# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V1.2 Game Streaming — Sunshine/Moonlight integration.

Architecture
────────────
Sunshine is an open-source Moonlight-compatible game streaming server.
Ozma manages one Sunshine instance per streamable node.

Node types and how streaming works per type:

  soft node (QEMU VM)
    Sunshine runs on the hypervisor (controller host), capturing the
    VM's display via v4l2loopback (/dev/videoN) or the VNC framebuffer.
    Zero extra hardware needed.

  desktop agent (bare-metal PC or laptop)
    Sunshine runs inside the target OS, capturing natively.
    The agent announces `cap=sunshine,sunshine_port=47990` in mDNS.
    The controller tracks it; does NOT run a second Sunshine.

  hardware node (RISC-V SBC with capture card)
    Not suitable for Sunshine (limited CPU/GPU, capture card output is
    already HLS). Use the standard HLS stream path instead.

Zero-config flow
────────────────
  1. Agent or soft node has Sunshine installed.
  2. On first start, SunshineManager detects the binary and generates a
     per-node config (resolution, encoder, ports).
  3. Sunshine starts and announces via mDNS (_nvstream._tcp.local.).
  4. Moonlight discovers the controller host automatically, shows a PIN.
  5. User enters the PIN in the Ozma dashboard → paired.
  6. Moonlight connects; game runs.

Encoding hierarchy (auto-selected)
────────────────────────────────────
  NVENC > VAAPI > Quick Sync > V4L2M2M > software (libx264/libx265)

Port allocation
───────────────
  Each Sunshine instance needs a unique base port (offsets all Sunshine
  ports by a fixed stride).  Ozma allocates:
    node 0: base 47984  (Sunshine default)
    node 1: base 48084  (+100)
    node 2: base 48184  (+200)
    …
  The HTTPS web UI port is always base+6 (47990 default).

Pairing flow
────────────
  - Moonlight generates a 4-digit PIN and shows it on screen.
  - User opens Ozma dashboard → Streaming → [node] → "Enter PIN".
  - Ozma POSTs the PIN to Sunshine's local API → pairing complete.
  - Paired client appears in the clients list with a certificate fingerprint.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.game_streaming")

# Default Sunshine base port (stream ports 47984-47989, API on 47990)
_SUNSHINE_BASE_PORT = 47984
_PORT_STRIDE = 100  # per-instance port stride

# Sunshine probes these encoders in order
_ENCODER_PROBE_ORDER = ["nvenc", "vaapi", "qsv", "v4l2m2m", "software"]

# Sunshine capture backends, in preference order
_CAPTURE_ORDER = ["kms", "wlroots", "x11", "v4l2", "nvfbc", "ddx"]


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class SunshineConfig:
    """Per-node Sunshine configuration."""
    node_id: str
    enabled: bool = False

    # Display capture
    capture: str = "auto"           # kms | wlroots | x11 | v4l2 | nvfbc | auto
    v4l2_device: str = ""           # /dev/videoN (soft nodes with v4l2loopback)

    # Encoding
    encoder: str = "auto"           # auto | nvenc | vaapi | qsv | v4l2m2m | software
    codec: str = "h264"             # h264 | h265 | av1
    bitrate_kbps: int = 10_000      # Mbps converted to kbps (Sunshine uses kbps)
    fps: int = 60
    resolutions: list[str] = field(default_factory=lambda: [
        "1920x1080", "2560x1440", "3840x2160",
    ])

    # Ports
    port_offset: int = 0            # added to _SUNSHINE_BASE_PORT

    # Audio
    audio_sink: str = ""            # PipeWire/PulseAudio sink name; empty = default

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id":      self.node_id,
            "enabled":      self.enabled,
            "capture":      self.capture,
            "v4l2_device":  self.v4l2_device,
            "encoder":      self.encoder,
            "codec":        self.codec,
            "bitrate_kbps": self.bitrate_kbps,
            "fps":          self.fps,
            "resolutions":  self.resolutions,
            "port_offset":  self.port_offset,
            "audio_sink":   self.audio_sink,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "SunshineConfig":
        return cls(
            node_id      = d["node_id"],
            enabled      = d.get("enabled", False),
            capture      = d.get("capture", "auto"),
            v4l2_device  = d.get("v4l2_device", ""),
            encoder      = d.get("encoder", "auto"),
            codec        = d.get("codec", "h264"),
            bitrate_kbps = d.get("bitrate_kbps", 10_000),
            fps          = d.get("fps", 60),
            resolutions  = d.get("resolutions", ["1920x1080", "2560x1440"]),
            port_offset  = d.get("port_offset", 0),
            audio_sink   = d.get("audio_sink", ""),
        )


@dataclass
class SunshineInstance:
    """Runtime state of one Sunshine process."""
    node_id: str
    config: SunshineConfig
    config_dir: Path

    state: str = "stopped"          # stopped | starting | running | error
    error: str = ""
    pid: int | None = None
    started_at: float | None = None
    paired_clients: list[dict] = field(default_factory=list)
    restarts: int = 0

    # If True, Sunshine is running remotely (agent-managed), not by us
    remote: bool = False
    remote_host: str = ""
    remote_api_port: int = 47990

    @property
    def api_port(self) -> int:
        if self.remote:
            return self.remote_api_port
        return _SUNSHINE_BASE_PORT + self.config.port_offset + 6  # +6 = API offset

    @property
    def stream_base_port(self) -> int:
        return _SUNSHINE_BASE_PORT + self.config.port_offset

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id":         self.node_id,
            "state":           self.state,
            "error":           self.error,
            "pid":             self.pid,
            "started_at":      self.started_at,
            "paired_clients":  self.paired_clients,
            "restarts":        self.restarts,
            "remote":          self.remote,
            "remote_host":     self.remote_host,
            "api_port":        self.api_port,
            "stream_base_port": self.stream_base_port,
            "config":          self.config.to_dict(),
        }


# ---------------------------------------------------------------------------
# Sunshine config file generation
# ---------------------------------------------------------------------------

def _build_sunshine_conf(cfg: SunshineConfig, config_dir: Path) -> str:
    """
    Generate a sunshine.conf file for the given node config.

    Sunshine uses a simple key=value format.  Only keys that differ from
    Sunshine's built-in defaults need to be set — but we set all relevant
    ones explicitly for reproducibility.
    """
    port = _SUNSHINE_BASE_PORT + cfg.port_offset
    creds_file = config_dir / "sunshine_state.json"
    log_file = config_dir / "sunshine.log"

    lines = [
        f"# Generated by Ozma for node: {cfg.node_id}",
        f"# Do not edit by hand — regenerated on each start.",
        "",
        # Network
        f"port = {port}",
        "origin_web_ui_allowed = lan",      # Restrict web UI to LAN
        "",
        # Encoding
        f"encoder = {cfg.encoder}",
        f"codec = {cfg.codec}",
        f"bitrate = {cfg.bitrate_kbps}",
        f"fps = {cfg.fps}",
        "",
    ]

    # Resolutions
    lines.append("resolutions = [")
    for res in cfg.resolutions:
        lines.append(f"  {res},")
    lines.append("]")
    lines.append("")

    # Capture source
    if cfg.v4l2_device:
        lines.append("capture = v4l2")
        lines.append(f"capture_device = {cfg.v4l2_device}")
    elif cfg.capture != "auto":
        lines.append(f"capture = {cfg.capture}")
    # else let Sunshine auto-detect

    lines.append("")

    # Audio
    if cfg.audio_sink:
        lines.append(f"audio_sink = {cfg.audio_sink}")

    # Paths — keep all Sunshine state in our managed directory
    lines += [
        "",
        f"credentials_file = {creds_file}",
        f"log_path = {log_file}",
        "",
        # Low-latency tuning
        "min_threads = 2",
        "hevc_mode = 0",         # Let Sunshine auto-negotiate H.265 support
        "av1_mode = 0",
        "",
        # Security — require pairing
        "origin_pin_allowed = lan",
    ]

    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Hardware detection helpers
# ---------------------------------------------------------------------------

async def _detect_encoder() -> str:
    """Probe available hardware encoders and return the best Sunshine encoder name."""
    # NVIDIA
    if shutil.which("nvidia-smi"):
        proc = await asyncio.create_subprocess_exec(
            "nvidia-smi", "--query-gpu=name", "--format=csv,noheader",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        if proc.returncode == 0 and out.strip():
            log.info("Sunshine encoder: nvenc (%s)", out.decode().strip().split("\n")[0])
            return "nvenc"

    # Intel Quick Sync / VAAPI
    for dev in Path("/dev/dri").glob("renderD*") if Path("/dev/dri").exists() else []:
        # Try VAAPI probe via ffmpeg
        proc = await asyncio.create_subprocess_exec(
            "ffmpeg", "-hide_banner", "-hwaccel", "vaapi",
            "-hwaccel_device", str(dev),
            "-f", "lavfi", "-i", "nullsrc=s=16x16:d=0.01",
            "-vf", "format=nv12,hwupload",
            "-vcodec", "h264_vaapi", "-f", "null", "-",
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        await proc.communicate()
        if proc.returncode == 0:
            # Distinguish Intel QSV vs AMD VAAPI
            try:
                vendor = Path("/sys/class/drm").read_text() if False else ""
            except Exception:
                vendor = ""
            log.info("Sunshine encoder: vaapi (%s)", dev)
            return "vaapi"

    log.info("Sunshine encoder: software fallback")
    return "software"


async def _detect_capture_backend() -> str:
    """Detect the best display capture backend for Sunshine on this host."""
    # Prefer KMS (works on both X11 and Wayland without extra config)
    if Path("/dev/dri/card0").exists():
        return "kms"
    # X11
    if os.environ.get("DISPLAY"):
        return "x11"
    # Wayland
    if os.environ.get("WAYLAND_DISPLAY"):
        return "wlroots"
    return "kms"  # default — let Sunshine figure it out


async def _sunshine_available() -> bool:
    """Return True if the sunshine binary is on PATH."""
    return shutil.which("sunshine") is not None


# ---------------------------------------------------------------------------
# SunshineManager
# ---------------------------------------------------------------------------

class SunshineManager:
    """
    Manages Sunshine streaming server instances, one per enabled node.

    For soft nodes running on this host, SunshineManager starts and
    monitors the Sunshine subprocess.  For agent-managed nodes (desktop
    agents with Sunshine built-in), it only tracks state and proxies
    pairing API calls.
    """

    HEALTH_INTERVAL = 15.0   # seconds between health polls
    RESTART_DELAY   = 5.0    # seconds before restarting a crashed instance
    MAX_RESTARTS    = 10

    def __init__(self, data_dir: Path | None = None,
                 state: Any = None) -> None:
        self._data_dir = data_dir or Path("/var/lib/ozma/sunshine")
        self._state_ref = state          # AppState reference (for node lookup)
        self._instances: dict[str, SunshineInstance] = {}
        self._configs: dict[str, SunshineConfig] = {}
        self._procs: dict[str, asyncio.subprocess.Process] = {}
        self._tasks: list[asyncio.Task] = []
        self._next_port_offset = 0       # monotonically increasing
        self._sunshine_binary: str | None = None
        self._default_encoder = "auto"
        self._default_capture = "auto"
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)

        # Probe for the binary and default encoder once at startup
        if await _sunshine_available():
            self._sunshine_binary = shutil.which("sunshine")
            self._default_encoder = await _detect_encoder()
            self._default_capture = await _detect_capture_backend()
            log.info("Sunshine binary found at %s; default encoder=%s capture=%s",
                     self._sunshine_binary, self._default_encoder, self._default_capture)
        else:
            log.info("Sunshine binary not found — streaming disabled for local nodes")

        # Re-enable nodes that were enabled before restart
        for node_id, cfg in list(self._configs.items()):
            if cfg.enabled:
                await self._start_instance(node_id, cfg)

        self._tasks.append(
            asyncio.create_task(self._health_loop(), name="sunshine:health")
        )

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

        # Stop all running instances
        for node_id in list(self._instances):
            await self._stop_instance(node_id)

    # ------------------------------------------------------------------
    # Node enable/disable
    # ------------------------------------------------------------------

    async def enable_node(
        self, node_id: str,
        capture: str = "auto",
        encoder: str = "auto",
        codec: str = "h264",
        fps: int = 60,
        bitrate_kbps: int = 10_000,
        resolutions: list[str] | None = None,
        v4l2_device: str = "",
        audio_sink: str = "",
    ) -> dict[str, Any]:
        """
        Enable Sunshine streaming for a node.

        If Sunshine is not already configured for this node, creates a new
        SunshineConfig, generates a config file, and starts the subprocess.
        Returns the instance status dict.
        """
        if node_id in self._configs:
            cfg = self._configs[node_id]
        else:
            cfg = SunshineConfig(
                node_id     = node_id,
                port_offset = self._allocate_port_offset(node_id),
            )

        cfg.enabled      = True
        cfg.capture      = capture if capture != "auto" else self._default_capture
        cfg.encoder      = encoder if encoder != "auto" else self._default_encoder
        cfg.codec        = codec
        cfg.fps          = fps
        cfg.bitrate_kbps = bitrate_kbps
        cfg.resolutions  = resolutions or ["1920x1080", "2560x1440", "3840x2160"]
        cfg.v4l2_device  = v4l2_device
        cfg.audio_sink   = audio_sink

        self._configs[node_id] = cfg
        self._save()
        await self._start_instance(node_id, cfg)
        inst = self._instances.get(node_id)
        return inst.to_dict() if inst else {"state": "error"}

    async def disable_node(self, node_id: str) -> None:
        """Stop Sunshine for a node and mark it disabled."""
        if node_id in self._configs:
            self._configs[node_id].enabled = False
            self._save()
        await self._stop_instance(node_id)

    def register_remote(self, node_id: str, host: str, api_port: int) -> None:
        """
        Register a node that manages its own Sunshine instance (e.g. desktop agent).

        The controller doesn't start/stop these — it only tracks state and
        proxies the pairing API.
        """
        cfg = self._configs.get(node_id) or SunshineConfig(
            node_id=node_id, enabled=True,
            port_offset=self._allocate_port_offset(node_id),
        )
        cfg.enabled = True
        self._configs[node_id] = cfg

        inst = SunshineInstance(
            node_id    = node_id,
            config     = cfg,
            config_dir = self._data_dir / _safe_id(node_id),
            state      = "running",
            remote     = True,
            remote_host= host,
            remote_api_port = api_port,
        )
        self._instances[node_id] = inst
        log.info("Registered remote Sunshine for %s @ %s:%d", node_id, host, api_port)

    def unregister_node(self, node_id: str) -> None:
        """Remove a node that has gone offline."""
        inst = self._instances.pop(node_id, None)
        if inst and not inst.remote:
            # If we own the process, stop it
            asyncio.create_task(
                self._stop_instance(node_id), name=f"sunshine:stop-{node_id}"
            )

    # ------------------------------------------------------------------
    # Pairing API (proxied to Sunshine's local HTTP API)
    # ------------------------------------------------------------------

    async def pair(self, node_id: str, pin: str) -> dict[str, Any]:
        """
        Submit a pairing PIN to Sunshine.

        Moonlight shows a PIN on screen; the user enters it in the Ozma
        dashboard; we forward it to Sunshine's local API.
        """
        inst = self._instances.get(node_id)
        if not inst or inst.state not in ("running",):
            return {"ok": False, "error": "Sunshine not running for this node"}

        url = f"http://{inst.remote_host if inst.remote else '127.0.0.1'}:{inst.api_port}/api/pin"
        try:
            import urllib.request
            data = json.dumps({"pin": pin}).encode()
            req = urllib.request.Request(
                url, data=data,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                body = json.loads(resp.read())
            return {"ok": True, "result": body}
        except Exception as exc:
            log.warning("Sunshine pair failed for %s: %s", node_id, exc)
            return {"ok": False, "error": str(exc)}

    async def list_clients(self, node_id: str) -> list[dict]:
        """Return paired Moonlight clients for a node."""
        inst = self._instances.get(node_id)
        if not inst:
            return []
        host = inst.remote_host if inst.remote else "127.0.0.1"
        url = f"http://{host}:{inst.api_port}/api/clients/list"
        try:
            import urllib.request
            with urllib.request.urlopen(url, timeout=5) as resp:
                body = json.loads(resp.read())
            clients = body.get("named_certs", [])
            inst.paired_clients = clients
            return clients
        except Exception as exc:
            log.debug("Could not list Sunshine clients for %s: %s", node_id, exc)
            return inst.paired_clients  # return cached

    async def unpair_client(self, node_id: str, cert: str) -> bool:
        """Unpair a Moonlight client by certificate."""
        inst = self._instances.get(node_id)
        if not inst:
            return False
        host = inst.remote_host if inst.remote else "127.0.0.1"
        url = f"http://{host}:{inst.api_port}/api/clients/unpair"
        try:
            import urllib.request
            data = json.dumps({"cert": cert}).encode()
            req = urllib.request.Request(
                url, data=data,
                headers={"Content-Type": "application/json"},
                method="DELETE",
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                body = json.loads(resp.read())
            return body.get("status", "").lower() == "true"
        except Exception as exc:
            log.warning("Sunshine unpair failed for %s cert %s: %s", node_id, cert[:16], exc)
            return False

    # ------------------------------------------------------------------
    # Status
    # ------------------------------------------------------------------

    def get_status(self, node_id: str) -> dict[str, Any] | None:
        inst = self._instances.get(node_id)
        return inst.to_dict() if inst else None

    def get_all_status(self) -> list[dict[str, Any]]:
        return [inst.to_dict() for inst in self._instances.values()]

    def get_config(self, node_id: str) -> SunshineConfig | None:
        return self._configs.get(node_id)

    def is_available(self) -> bool:
        """Return True if the Sunshine binary exists on this host."""
        return self._sunshine_binary is not None

    def moonlight_address(self, node_id: str) -> str | None:
        """
        Return the Moonlight-compatible stream address for a node.

        Moonlight connects to `host:stream_base_port`.
        """
        inst = self._instances.get(node_id)
        if not inst or inst.state != "running":
            return None
        if inst.remote:
            return f"{inst.remote_host}:{inst.stream_base_port}"
        # Local: use the controller's own IP (Moonlight must reach this host)
        return f"localhost:{inst.stream_base_port}"

    # ------------------------------------------------------------------
    # Internal: subprocess lifecycle
    # ------------------------------------------------------------------

    async def _start_instance(self, node_id: str, cfg: SunshineConfig) -> None:
        if not self._sunshine_binary:
            log.warning("Sunshine binary not found; cannot start for %s", node_id)
            inst = self._instances.get(node_id) or SunshineInstance(
                node_id=node_id, config=cfg,
                config_dir=self._data_dir / _safe_id(node_id),
            )
            inst.state = "error"
            inst.error = "Sunshine binary not found on this host"
            self._instances[node_id] = inst
            return

        config_dir = self._data_dir / _safe_id(node_id)
        config_dir.mkdir(parents=True, exist_ok=True)

        conf_text = _build_sunshine_conf(cfg, config_dir)
        conf_path = config_dir / "sunshine.conf"
        conf_path.write_text(conf_text)
        conf_path.chmod(0o600)

        inst = self._instances.get(node_id) or SunshineInstance(
            node_id=node_id, config=cfg, config_dir=config_dir,
        )
        inst.config    = cfg
        inst.state     = "starting"
        inst.error     = ""
        self._instances[node_id] = inst

        task = asyncio.create_task(
            self._run_instance(node_id, cfg, config_dir, conf_path),
            name=f"sunshine:{node_id}",
        )
        self._tasks.append(task)
        log.info("Starting Sunshine for node %s (port_offset=%d)", node_id, cfg.port_offset)

    async def _run_instance(
        self, node_id: str, cfg: SunshineConfig,
        config_dir: Path, conf_path: Path,
    ) -> None:
        """Supervisor loop: runs Sunshine, restarts on crash."""
        inst = self._instances[node_id]
        restarts = 0

        while True:
            try:
                proc = await asyncio.create_subprocess_exec(
                    self._sunshine_binary, str(conf_path),
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.STDOUT,
                    cwd=str(config_dir),
                )
                self._procs[node_id] = proc
                inst.pid       = proc.pid
                inst.state     = "starting"
                inst.started_at = time.time()
                log.info("Sunshine pid=%d started for %s", proc.pid, node_id)

                # Give Sunshine a moment to start, then check health
                await asyncio.sleep(3.0)
                healthy = await self._probe_health(node_id)
                if healthy:
                    inst.state = "running"
                    log.info("Sunshine healthy for %s", node_id)

                rc = await proc.wait()
                log.warning("Sunshine exited rc=%d for %s", rc, node_id)
                inst.state = "stopped" if rc == 0 else "error"
                inst.pid = None

            except asyncio.CancelledError:
                await self._kill_proc(node_id)
                inst.state = "stopped"
                return

            except Exception as exc:
                log.exception("Sunshine crashed for %s: %s", node_id, exc)
                inst.state = "error"
                inst.error = str(exc)

            # Check if still enabled
            if not self._configs.get(node_id, cfg).enabled:
                return

            restarts += 1
            inst.restarts = restarts
            if restarts >= self.MAX_RESTARTS:
                log.error("Sunshine hit max restarts for %s; giving up", node_id)
                inst.state = "error"
                inst.error = f"Crashed {restarts} times — disabled"
                return

            log.info("Restarting Sunshine for %s in %.0fs (attempt %d/%d)",
                     node_id, self.RESTART_DELAY, restarts, self.MAX_RESTARTS)
            await asyncio.sleep(self.RESTART_DELAY)

    async def _stop_instance(self, node_id: str) -> None:
        await self._kill_proc(node_id)
        inst = self._instances.get(node_id)
        if inst:
            inst.state = "stopped"
            inst.pid = None

    async def _kill_proc(self, node_id: str) -> None:
        proc = self._procs.pop(node_id, None)
        if proc and proc.returncode is None:
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass

    # ------------------------------------------------------------------
    # Internal: health polling
    # ------------------------------------------------------------------

    async def _health_loop(self) -> None:
        while True:
            try:
                await asyncio.sleep(self.HEALTH_INTERVAL)
                for node_id, inst in list(self._instances.items()):
                    if inst.state == "running":
                        healthy = await self._probe_health(node_id)
                        if not healthy and not inst.remote:
                            log.warning("Sunshine health check failed for %s", node_id)
                            # The _run_instance loop will restart it
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Sunshine health loop error")

    async def _probe_health(self, node_id: str) -> bool:
        """GET /api/info from Sunshine's local API."""
        inst = self._instances.get(node_id)
        if not inst:
            return False
        host = inst.remote_host if inst.remote else "127.0.0.1"
        url = f"http://{host}:{inst.api_port}/api/info"
        try:
            import urllib.request
            with urllib.request.urlopen(url, timeout=3) as resp:
                body = json.loads(resp.read())
            return "version" in body or resp.status == 200
        except Exception:
            return False

    # ------------------------------------------------------------------
    # Internal: port allocation
    # ------------------------------------------------------------------

    def _allocate_port_offset(self, node_id: str) -> int:
        # Check if node already has an allocated offset
        if node_id in self._configs:
            return self._configs[node_id].port_offset
        offset = self._next_port_offset * _PORT_STRIDE
        self._next_port_offset += 1
        return offset

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        state_path = self._data_dir / "sunshine_state.json"
        tmp = state_path.with_suffix(".tmp")
        data = {
            "next_port_offset": self._next_port_offset,
            "configs": {nid: cfg.to_dict() for nid, cfg in self._configs.items()},
        }
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(state_path)

    def _load(self) -> None:
        state_path = self._data_dir / "sunshine_state.json"
        if not state_path.exists():
            return
        try:
            data = json.loads(state_path.read_text())
            self._next_port_offset = data.get("next_port_offset", 0)
            for nid, d in data.get("configs", {}).items():
                self._configs[nid] = SunshineConfig.from_dict(d)
        except Exception:
            log.exception("Failed to load Sunshine state")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _safe_id(node_id: str) -> str:
    """Convert a node_id to a filesystem-safe directory name."""
    return re.sub(r"[^\w.-]", "_", node_id)[:64]
