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
  GET  /metrics                    Prometheus text format
  GET  /health                     Health check
"""

from __future__ import annotations

import asyncio
import logging
from typing import TYPE_CHECKING, Any

from aiohttp import web

if TYPE_CHECKING:
    from .seat_manager import SeatManager

log = logging.getLogger("ozma.agent.multiseat.monitoring")

DEFAULT_PORT = 7399


class MonitoringServer:
    """Management HTTP server for multi-seat monitoring and diagnostics."""

    def __init__(self, seat_manager: SeatManager, port: int = DEFAULT_PORT) -> None:
        self._manager = seat_manager
        self._port = port
        self._runner: web.AppRunner | None = None

    async def start(self) -> None:
        """Start the monitoring HTTP server."""
        app = web.Application()
        app.router.add_get("/api/v1/gpus", self._handle_gpus)
        app.router.add_get("/api/v1/encoders", self._handle_encoders)
        app.router.add_get("/api/v1/encoders/history", self._handle_encoder_history)
        app.router.add_post("/api/v1/encoders/rebalance", self._handle_rebalance)
        app.router.add_get("/api/v1/seats", self._handle_seats)
        app.router.add_get("/api/v1/games", self._handle_games)
        app.router.add_post("/api/v1/seats/{seat}/launch", self._handle_launch)
        app.router.add_post("/api/v1/seats/{seat}/stop-game", self._handle_stop_game)
        app.router.add_get("/api/v1/seats/{seat}/game", self._handle_seat_game)
        app.router.add_get("/api/v1/hotplug", self._handle_hotplug)
        app.router.add_get("/metrics", self._handle_metrics)
        app.router.add_get("/health", self._handle_health)

        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "0.0.0.0", self._port)
        await site.start()
        log.info("Monitoring server listening on port %d", self._port)

    async def stop(self) -> None:
        """Stop the monitoring HTTP server."""
        if self._runner:
            await self._runner.cleanup()
            self._runner = None
        log.info("Monitoring server stopped")

    # ── Handlers ────────────────────────────────────────────────────────────

    async def _handle_gpus(self, _request: web.Request) -> web.Response:
        """GET /api/v1/gpus -- GPU inventory with active session counts."""
        inventory = self._manager._gpu_inventory
        allocator = self._manager._encoder_allocator

        gpus = []
        for gpu in inventory.gpus:
            encoders = []
            for enc in gpu.encoders:
                active = allocator.active_sessions_on(gpu.index, enc.name) if allocator else 0
                encoders.append({
                    "name": enc.name,
                    "codec": enc.codec,
                    "max_sessions": enc.max_sessions,
                    "active_sessions": active,
                    "quality": enc.quality,
                    "latency": enc.latency,
                })
            gpus.append({
                "index": gpu.index,
                "name": gpu.name,
                "vendor": gpu.vendor,
                "is_igpu": gpu.is_igpu,
                "pci_slot": gpu.pci_slot,
                "vram_mb": gpu.vram_mb,
                "render_device": gpu.render_device,
                "encoders": encoders,
            })

        return web.json_response({"gpus": gpus})

    async def _handle_encoders(self, _request: web.Request) -> web.Response:
        """GET /api/v1/encoders -- current encoder allocations."""
        allocator = self._manager._encoder_allocator
        if not allocator:
            return web.json_response({"allocations": [], "sessions": {}})

        inventory = self._manager._gpu_inventory
        allocations = []
        for seat_name, session in allocator.sessions.items():
            gpu = inventory.gpu_by_index(session.gpu_index)
            gpu_name = gpu.name if gpu else "software"
            allocations.append({
                "seat": seat_name,
                "encoder": session.encoder.name,
                "gpu_index": session.gpu_index,
                "gpu_name": gpu_name,
                "score": session.score,
                "reason": session.reason,
                "ffmpeg_args": session.ffmpeg_args,
            })

        # Session counts keyed by "encoder_name:gpu_index"
        sessions: dict[str, dict[str, int]] = {}
        for gpu in inventory.gpus:
            for enc in gpu.encoders:
                key = f"{enc.name}:{gpu.index}"
                active = allocator.active_sessions_on(gpu.index, enc.name)
                sessions[key] = {"active": active, "max": enc.max_sessions}

        return web.json_response({"allocations": allocations, "sessions": sessions})

    async def _handle_encoder_history(self, _request: web.Request) -> web.Response:
        """GET /api/v1/encoders/history -- recent allocation decisions."""
        allocator = self._manager._encoder_allocator
        if not allocator:
            return web.json_response({"events": []})

        return web.json_response({"events": allocator.get_history()})

    async def _handle_rebalance(self, _request: web.Request) -> web.Response:
        """POST /api/v1/encoders/rebalance -- force rebalance."""
        reassigned = await self._manager.rebalance_encoders()
        # Return the current allocations after rebalance
        allocator = self._manager._encoder_allocator
        inventory = self._manager._gpu_inventory
        allocations = []
        if allocator:
            for seat_name, session in allocator.sessions.items():
                gpu = inventory.gpu_by_index(session.gpu_index)
                gpu_name = gpu.name if gpu else "software"
                allocations.append({
                    "seat": seat_name,
                    "encoder": session.encoder.name,
                    "gpu_index": session.gpu_index,
                    "gpu_name": gpu_name,
                    "score": session.score,
                    "reason": session.reason,
                })

        return web.json_response({
            "reassigned": reassigned,
            "allocations": allocations,
        })

    async def _handle_seats(self, _request: web.Request) -> web.Response:
        """GET /api/v1/seats -- all seats with encoder info."""
        allocator = self._manager._encoder_allocator
        inventory = self._manager._gpu_inventory
        seats = []

        for seat in self._manager.seats:
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
                        "name": session.encoder.name,
                        "gpu": gpu_name,
                        "gpu_index": session.gpu_index,
                        "sessions": sessions_str,
                        "score": session.score,
                        "reason": session.reason,
                    }

            display_name = seat.display.name if seat.display else None
            capture_active = (
                seat._screen_proc is not None
                and seat._screen_proc.returncode is None
            )

            seats.append({
                "name": seat.name,
                "index": seat.seat_index,
                "display": display_name,
                "udp_port": seat.udp_port,
                "api_port": seat.api_port,
                "encoder": encoder_info,
                "audio_sink": seat.audio_sink,
                "input_devices": seat.input_devices,
                "status": "running" if capture_active else "idle",
            })

        return web.json_response({"seats": seats})

    async def _handle_games(self, request: web.Request) -> web.Response:
        """GET /api/v1/games -- discovered game library."""
        launcher = self._manager.game_launcher
        if not launcher:
            return web.json_response({"games": []})

        # Allow ?refresh=1 to force rescan
        if request.query.get("refresh") == "1":
            await launcher.discover_games()

        games = [g.to_dict() for g in launcher.games]
        return web.json_response({"games": games, "count": len(games)})

    async def _handle_launch(self, request: web.Request) -> web.Response:
        """POST /api/v1/seats/{seat}/launch -- launch a game on a seat."""
        seat_name = request.match_info["seat"]
        seat = self._manager.get_seat(seat_name)
        if not seat:
            return web.json_response(
                {"error": f"seat not found: {seat_name}"}, status=404,
            )

        launcher = self._manager.game_launcher
        if not launcher:
            return web.json_response(
                {"error": "game launcher not initialized"}, status=503,
            )

        try:
            body = await request.json()
        except Exception:
            return web.json_response(
                {"error": "invalid JSON body"}, status=400,
            )

        game_id = body.get("game_id")
        if not game_id:
            return web.json_response(
                {"error": "game_id required"}, status=400,
            )

        # Find the game
        game = None
        for g in launcher.games:
            if g.id == game_id:
                game = g
                break

        if not game:
            return web.json_response(
                {"error": f"game not found: {game_id}"}, status=404,
            )

        proc = await launcher.launch(game, seat)
        if proc:
            return web.json_response({
                "ok": True,
                "game": game.to_dict(),
                "pid": proc.pid,
                "seat": seat_name,
            })
        return web.json_response(
            {"error": "failed to launch game"}, status=500,
        )

    async def _handle_stop_game(self, request: web.Request) -> web.Response:
        """POST /api/v1/seats/{seat}/stop-game -- stop game on a seat."""
        seat_name = request.match_info["seat"]
        seat = self._manager.get_seat(seat_name)
        if not seat:
            return web.json_response(
                {"error": f"seat not found: {seat_name}"}, status=404,
            )

        launcher = self._manager.game_launcher
        if not launcher:
            return web.json_response(
                {"error": "game launcher not initialized"}, status=503,
            )

        stopped = await launcher.stop(seat)
        return web.json_response({"ok": stopped, "seat": seat_name})

    async def _handle_seat_game(self, request: web.Request) -> web.Response:
        """GET /api/v1/seats/{seat}/game -- what's running on a seat."""
        seat_name = request.match_info["seat"]
        seat = self._manager.get_seat(seat_name)
        if not seat:
            return web.json_response(
                {"error": f"seat not found: {seat_name}"}, status=404,
            )

        launcher = self._manager.game_launcher
        if not launcher:
            return web.json_response({"game": None})

        state = launcher.running_state(seat_name)
        return web.json_response({"game": state})

    async def _handle_hotplug(self, _request: web.Request) -> web.Response:
        """GET /api/v1/hotplug -- hotplug monitor state."""
        hotplug = self._manager.hotplug
        if not hotplug:
            return web.json_response({"enabled": False})

        result = hotplug.to_dict()
        result["enabled"] = True
        return web.json_response(result)

    async def _handle_metrics(self, _request: web.Request) -> web.Response:
        """GET /metrics -- Prometheus text exposition format."""
        lines = _build_prometheus_metrics(self._manager)
        return web.Response(
            text="".join(lines),
            content_type="text/plain; version=0.0.4; charset=utf-8",
        )

    async def _handle_health(self, _request: web.Request) -> web.Response:
        """GET /health -- simple health check."""
        return web.json_response({
            "ok": True,
            "seats": self._manager.seat_count,
            "gpus": len(self._manager._gpu_inventory.gpus),
        })


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
