# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Multi-seat monitoring HTTP server.

Serves operational endpoints for the GPU inventory, encoder allocator,
seat status, game launcher, and hotplug state. Runs on port 7399 as
the multi-seat management API.

Endpoints:
  GET  /api/v1/gpus                GPU inventory
  GET  /api/v1/encoders            Current encoder allocations
  GET  /api/v1/encoders/history    Allocation decision history (last 100)
  POST /api/v1/encoders/rebalance  Force encoder rebalance
  GET  /api/v1/seats               All seats with encoder info
  GET  /api/v1/games               Discovered game library
  POST /api/v1/seats/{seat}/launch Launch a game on a seat
  POST /api/v1/seats/{seat}/stop-game  Stop game on a seat
  GET  /api/v1/seats/{seat}/game   What's running on a seat
  GET  /api/v1/hotplug             Hotplug monitor state
  GET  /api/v1/isolation            Available backends + per-seat status
  GET  /metrics                    Prometheus text format
  GET  /health                     Health check
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .seat_manager import SeatManager

log = logging.getLogger("ozma.agent.multiseat.monitoring")

DEFAULT_PORT = 7399


class MonitoringServer:
    """Management HTTP server for multi-seat monitoring and diagnostics.

    Uses stdlib ``http.server`` in a daemon thread instead of aiohttp,
    so it works on Windows with ProactorEventLoop.
    """

    def __init__(self, seat_manager: SeatManager, port: int = DEFAULT_PORT) -> None:
        self._manager = seat_manager
        self._port = port
        self._server: HTTPServer | None = None
        self._loop: asyncio.AbstractEventLoop | None = None

    async def start(self) -> None:
        """Start the monitoring HTTP server in a daemon thread."""
        self._loop = asyncio.get_running_loop()
        manager = self._manager
        loop = self._loop

        # Regex patterns for seat-specific routes
        _SEAT_LAUNCH = re.compile(r"^/api/v1/seats/([^/]+)/launch$")
        _SEAT_STOP = re.compile(r"^/api/v1/seats/([^/]+)/stop-game$")
        _SEAT_GAME = re.compile(r"^/api/v1/seats/([^/]+)/game$")

        class _Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                path = self.path.split("?")[0]
                query = self.path.split("?", 1)[1] if "?" in self.path else ""

                if path == "/api/v1/gpus":
                    self._json(200, _get_gpus(manager))
                elif path == "/api/v1/encoders":
                    self._json(200, _get_encoders(manager))
                elif path == "/api/v1/encoders/history":
                    self._json(200, _get_encoder_history(manager))
                elif path == "/api/v1/seats":
                    self._json(200, _get_seats(manager))
                elif path == "/api/v1/games":
                    refresh = "refresh=1" in query
                    if refresh and manager.game_launcher:
                        _run_coro(loop, manager.game_launcher.discover_games())
                    self._json(200, _get_games(manager))
                elif path == "/api/v1/hotplug":
                    self._json(200, _get_hotplug(manager))
                elif path == "/api/v1/isolation":
                    self._json(200, manager.isolation_manager.to_dict())
                elif path == "/metrics":
                    self._text(200, "".join(_build_prometheus_metrics(manager)),
                               "text/plain; version=0.0.4; charset=utf-8")
                elif path == "/health":
                    self._json(200, {
                        "ok": True,
                        "seats": manager.seat_count,
                        "gpus": len(manager._gpu_inventory.gpus),
                    })
                else:
                    # Seat-specific GET routes
                    m = _SEAT_GAME.match(path)
                    if m:
                        self._json(200, _get_seat_game(manager, m.group(1)))
                        return
                    self.send_error(404)

            def do_POST(self) -> None:  # noqa: N802
                path = self.path.split("?")[0]

                if path == "/api/v1/encoders/rebalance":
                    result = _run_coro(loop, manager.rebalance_encoders())
                    self._json(200, _get_rebalance_result(manager, result))
                    return

                m = _SEAT_LAUNCH.match(path)
                if m:
                    self._handle_launch(m.group(1))
                    return

                m = _SEAT_STOP.match(path)
                if m:
                    self._handle_stop_game(m.group(1))
                    return

                self.send_error(404)

            # ── helpers ────────────────────────────────────────

            def _json(self, status: int, data: Any) -> None:
                body = json.dumps(data).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _text(self, status: int, text: str, ctype: str) -> None:
                body = text.encode()
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def _read_body(self) -> bytes:
                length = int(self.headers.get("Content-Length", 0))
                return self.rfile.read(length) if length else b""

            def _handle_launch(self, seat_name: str) -> None:
                seat = manager.get_seat(seat_name)
                if not seat:
                    self._json(404, {"error": f"seat not found: {seat_name}"})
                    return
                launcher = manager.game_launcher
                if not launcher:
                    self._json(503, {"error": "game launcher not initialized"})
                    return
                try:
                    body = json.loads(self._read_body())
                except Exception:
                    self._json(400, {"error": "invalid JSON body"})
                    return
                game_id = body.get("game_id")
                if not game_id:
                    self._json(400, {"error": "game_id required"})
                    return
                game = next((g for g in launcher.games if g.id == game_id), None)
                if not game:
                    self._json(404, {"error": f"game not found: {game_id}"})
                    return
                proc = _run_coro(loop, launcher.launch(game, seat))
                if proc:
                    self._json(200, {
                        "ok": True, "game": game.to_dict(),
                        "pid": proc.pid, "seat": seat_name,
                    })
                else:
                    self._json(500, {"error": "failed to launch game"})

            def _handle_stop_game(self, seat_name: str) -> None:
                seat = manager.get_seat(seat_name)
                if not seat:
                    self._json(404, {"error": f"seat not found: {seat_name}"})
                    return
                launcher = manager.game_launcher
                if not launcher:
                    self._json(503, {"error": "game launcher not initialized"})
                    return
                stopped = _run_coro(loop, launcher.stop(seat))
                self._json(200, {"ok": stopped, "seat": seat_name})

            def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
                pass  # suppress default stderr logging

        server = HTTPServer(("0.0.0.0", self._port), _Handler)
        self._server = server
        thread = threading.Thread(
            target=server.serve_forever, daemon=True,
            name="monitoring-http",
        )
        thread.start()
        log.info("Monitoring server listening on port %d", self._port)

    def start_sync(self) -> None:
        """Start server synchronously (for tests). No event loop needed."""
        # We need to run the async start without an event loop by inlining the logic
        asyncio.run(self.start())

    def stop_sync(self) -> None:
        """Stop server synchronously (for tests)."""
        if self._server:
            self._server.shutdown()
            self._server = None

    async def stop(self) -> None:
        """Stop the monitoring HTTP server."""
        if self._server:
            self._server.shutdown()
            self._server = None
        log.info("Monitoring server stopped")


def _run_coro(loop: asyncio.AbstractEventLoop, coro: Any) -> Any:
    """Run an asyncio coroutine from a synchronous thread, blocking until done."""
    import concurrent.futures
    future = asyncio.run_coroutine_threadsafe(coro, loop)
    return future.result(timeout=30)


# ── Data‐gathering helpers (called from handler thread) ─────────────────────

def _get_gpus(manager: SeatManager) -> dict:
    inventory = manager._gpu_inventory
    allocator = manager._encoder_allocator
    gpus = []
    for gpu in inventory.gpus:
        encoders = []
        for enc in gpu.encoders:
            active = allocator.active_sessions_on(gpu.index, enc.name) if allocator else 0
            encoders.append({
                "name": enc.name, "codec": enc.codec,
                "max_sessions": enc.max_sessions, "active_sessions": active,
                "quality": enc.quality, "latency": enc.latency,
            })
        gpus.append({
            "index": gpu.index, "name": gpu.name, "vendor": gpu.vendor,
            "is_igpu": gpu.is_igpu, "pci_slot": gpu.pci_slot,
            "vram_mb": gpu.vram_mb, "render_device": gpu.render_device,
            "encoders": encoders,
        })
    return {"gpus": gpus}


def _get_encoders(manager: SeatManager) -> dict:
    allocator = manager._encoder_allocator
    if not allocator:
        return {"allocations": [], "sessions": {}}
    inventory = manager._gpu_inventory
    allocations = []
    for seat_name, session in allocator.sessions.items():
        gpu = inventory.gpu_by_index(session.gpu_index)
        gpu_name = gpu.name if gpu else "software"
        allocations.append({
            "seat": seat_name, "encoder": session.encoder.name,
            "gpu_index": session.gpu_index, "gpu_name": gpu_name,
            "score": session.score, "reason": session.reason,
            "ffmpeg_args": session.ffmpeg_args,
        })
    sessions: dict[str, dict[str, int]] = {}
    for gpu in inventory.gpus:
        for enc in gpu.encoders:
            key = f"{enc.name}:{gpu.index}"
            active = allocator.active_sessions_on(gpu.index, enc.name)
            sessions[key] = {"active": active, "max": enc.max_sessions}
    return {"allocations": allocations, "sessions": sessions}


def _get_encoder_history(manager: SeatManager) -> dict:
    allocator = manager._encoder_allocator
    if not allocator:
        return {"events": []}
    return {"events": allocator.get_history()}


def _get_rebalance_result(manager: SeatManager, reassigned: list[str]) -> dict:
    allocator = manager._encoder_allocator
    inventory = manager._gpu_inventory
    allocations = []
    if allocator:
        for seat_name, session in allocator.sessions.items():
            gpu = inventory.gpu_by_index(session.gpu_index)
            gpu_name = gpu.name if gpu else "software"
            allocations.append({
                "seat": seat_name, "encoder": session.encoder.name,
                "gpu_index": session.gpu_index, "gpu_name": gpu_name,
                "score": session.score, "reason": session.reason,
            })
    return {"reassigned": reassigned, "allocations": allocations}


def _get_seats(manager: SeatManager) -> dict:
    allocator = manager._encoder_allocator
    inventory = manager._gpu_inventory
    seats = []
    for seat in manager.seats:
        encoder_info: dict[str, Any] | None = None
        if allocator:
            session = allocator.sessions.get(seat.name)
            if session:
                gpu = inventory.gpu_by_index(session.gpu_index)
                gpu_name = gpu.name if gpu else "software"
                max_s = session.encoder.max_sessions
                active_s = allocator.active_sessions_on(
                    session.gpu_index, session.encoder.name,
                )
                sessions_str = (
                    f"{active_s}/unlimited" if max_s < 0
                    else f"{active_s}/{max_s}"
                )
                encoder_info = {
                    "name": session.encoder.name, "gpu": gpu_name,
                    "gpu_index": session.gpu_index, "sessions": sessions_str,
                    "score": session.score, "reason": session.reason,
                }
        display_name = seat.display.name if seat.display else None
        capture_active = (
            seat._screen_proc is not None
            and seat._screen_proc.returncode is None
        )
        iso_ctx = manager.isolation_manager.get_context(seat.name)
        isolation_info = iso_ctx.to_dict() if iso_ctx else {"backend": "none"}
        seats.append({
            "name": seat.name, "index": seat.seat_index,
            "display": display_name, "udp_port": seat.udp_port,
            "api_port": seat.api_port, "encoder": encoder_info,
            "audio_sink": seat.audio_sink,
            "input_devices": seat.input_devices,
            "isolation": isolation_info,
            "status": "running" if capture_active else "idle",
        })
    return {"seats": seats}


def _get_games(manager: SeatManager) -> dict:
    launcher = manager.game_launcher
    if not launcher:
        return {"games": []}
    games = [g.to_dict() for g in launcher.games]
    return {"games": games, "count": len(games)}


def _get_seat_game(manager: SeatManager, seat_name: str) -> dict:
    seat = manager.get_seat(seat_name)
    if not seat:
        return {"error": f"seat not found: {seat_name}"}
    launcher = manager.game_launcher
    if not launcher:
        return {"game": None}
    state = launcher.running_state(seat_name)
    return {"game": state}


def _get_hotplug(manager: SeatManager) -> dict:
    hotplug = manager.hotplug
    if not hotplug:
        return {"enabled": False}
    result = hotplug.to_dict()
    result["enabled"] = True
    return result


# ── Prometheus metric formatting ──────────────────────────────────────────────

def _prom_escape(value: str) -> str:
    """Escape a label value for Prometheus text format."""
    return value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def _gauge(name: str, help_text: str, value: Any, labels: str = "") -> str:
    label_str = f"{{{labels}}}" if labels else ""
    return f"# HELP {name} {help_text}\n# TYPE {name} gauge\n{name}{label_str} {value}\n"


def _gauge_line(name: str, value: Any, labels: str = "") -> str:
    """A single gauge line without HELP/TYPE (for multi-value gauges)."""
    label_str = f"{{{labels}}}" if labels else ""
    return f"{name}{label_str} {value}\n"


def _build_prometheus_metrics(manager: SeatManager) -> list[str]:
    """Build all Prometheus metrics lines."""
    lines: list[str] = []
    inventory = manager._gpu_inventory
    allocator = manager._encoder_allocator

    # GPU count
    lines.append(_gauge("ozma_gpu_count", "Number of GPUs detected",
                        len(inventory.gpus)))

    # Seat count
    lines.append(_gauge("ozma_seat_count", "Active seats", manager.seat_count))

    # Encoder sessions per GPU
    lines.append(f"# HELP ozma_encoder_sessions Active encoder sessions per GPU\n")
    lines.append(f"# TYPE ozma_encoder_sessions gauge\n")
    for gpu in inventory.gpus:
        gpu_label = _prom_escape(gpu.name)
        for enc in gpu.encoders:
            active = allocator.active_sessions_on(gpu.index, enc.name) if allocator else 0
            labels = (
                f'gpu="{gpu.index}",encoder="{_prom_escape(enc.name)}",'
                f'gpu_name="{gpu_label}"'
            )
            lines.append(_gauge_line("ozma_encoder_sessions", active, labels))

    # Encoder session limits
    lines.append(f"# HELP ozma_encoder_session_limit Max encoder sessions per GPU (-1 = unlimited)\n")
    lines.append(f"# TYPE ozma_encoder_session_limit gauge\n")
    for gpu in inventory.gpus:
        gpu_label = _prom_escape(gpu.name)
        for enc in gpu.encoders:
            labels = (
                f'gpu="{gpu.index}",encoder="{_prom_escape(enc.name)}",'
                f'gpu_name="{gpu_label}"'
            )
            lines.append(_gauge_line("ozma_encoder_session_limit",
                                     enc.max_sessions, labels))

    # Per-seat encoder assignment (info-style gauge = 1)
    lines.append(f"# HELP ozma_seat_encoder Encoder assignment per seat (value=1)\n")
    lines.append(f"# TYPE ozma_seat_encoder gauge\n")
    if allocator:
        for seat_name, session in allocator.sessions.items():
            labels = (
                f'seat="{_prom_escape(seat_name)}",'
                f'encoder="{_prom_escape(session.encoder.name)}",'
                f'gpu="{session.gpu_index}"'
            )
            lines.append(_gauge_line("ozma_seat_encoder", 1, labels))

    # Per-seat encoder score (useful for dashboards)
    lines.append(f"# HELP ozma_seat_encoder_score Encoder allocation score per seat\n")
    lines.append(f"# TYPE ozma_seat_encoder_score gauge\n")
    if allocator:
        for seat_name, session in allocator.sessions.items():
            labels = (
                f'seat="{_prom_escape(seat_name)}",'
                f'encoder="{_prom_escape(session.encoder.name)}"'
            )
            lines.append(_gauge_line("ozma_seat_encoder_score",
                                     session.score, labels))

    # GPU VRAM
    lines.append(f"# HELP ozma_gpu_vram_mb GPU VRAM in megabytes (0 for iGPU)\n")
    lines.append(f"# TYPE ozma_gpu_vram_mb gauge\n")
    for gpu in inventory.gpus:
        labels = (
            f'gpu="{gpu.index}",gpu_name="{_prom_escape(gpu.name)}",'
            f'vendor="{_prom_escape(gpu.vendor)}",'
            f'is_igpu="{gpu.is_igpu}"'
        )
        lines.append(_gauge_line("ozma_gpu_vram_mb", gpu.vram_mb, labels))

    # Allocation history count
    history_len = len(allocator.get_history()) if allocator else 0
    lines.append(_gauge("ozma_encoder_allocation_events_total",
                        "Total encoder allocation events in history buffer",
                        history_len))

    # Game library size
    launcher = manager.game_launcher
    if launcher:
        lines.append(_gauge("ozma_game_library_size",
                            "Number of discovered games", len(launcher.games)))

        # Running games per seat
        lines.append("# HELP ozma_seat_game_running Whether a game is running on a seat (1=yes)\n")
        lines.append("# TYPE ozma_seat_game_running gauge\n")
        for seat in manager.seats:
            running = launcher.running_state(seat.name)
            value = 1 if running and running.get("alive") else 0
            labels = f'seat="{_prom_escape(seat.name)}"'
            if running:
                labels += f',game="{_prom_escape(running["game"]["name"])}"'
            lines.append(_gauge_line("ozma_seat_game_running", value, labels))

    # Hotplug state
    hotplug = manager.hotplug
    if hotplug:
        lines.append(_gauge("ozma_hotplug_known_groups",
                            "Number of known USB input groups",
                            len(hotplug._known_groups)))
        lines.append(_gauge("ozma_hotplug_pending_removals",
                            "Number of seats in removal grace period",
                            len(hotplug._pending_removals)))

    return lines
