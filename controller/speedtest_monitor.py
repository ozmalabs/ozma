# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""WAN speed monitoring — runs speedtest-cli / librespeed-cli / iperf3 on a schedule."""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.speedtest")


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class SpeedtestResult:
    timestamp: float
    download_mbps: float
    upload_mbps: float
    ping_ms: float
    server_name: str = ""
    server_host: str = ""
    isp: str = ""
    result_url: str = ""
    tool: str = "speedtest-cli"

    def to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def from_dict(cls, d: dict) -> "SpeedtestResult":
        return cls(
            timestamp=d["timestamp"],
            download_mbps=d["download_mbps"],
            upload_mbps=d["upload_mbps"],
            ping_ms=d["ping_ms"],
            server_name=d.get("server_name", ""),
            server_host=d.get("server_host", ""),
            isp=d.get("isp", ""),
            result_url=d.get("result_url", ""),
            tool=d.get("tool", "speedtest-cli"),
        )


@dataclass
class SpeedtestConfig:
    enabled: bool = False
    interval_hours: float = 6.0
    tool: str = "auto"
    iperf3_server: str = ""
    min_download_mbps: float = 0.0
    min_upload_mbps: float = 0.0
    max_ping_ms: float = 0.0
    history_max: int = 168

    def to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def from_dict(cls, d: dict) -> "SpeedtestConfig":
        return cls(
            enabled=d.get("enabled", False),
            interval_hours=d.get("interval_hours", 6.0),
            tool=d.get("tool", "auto"),
            iperf3_server=d.get("iperf3_server", ""),
            min_download_mbps=d.get("min_download_mbps", 0.0),
            min_upload_mbps=d.get("min_upload_mbps", 0.0),
            max_ping_ms=d.get("max_ping_ms", 0.0),
            history_max=d.get("history_max", 168),
        )


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class SpeedtestMonitor:
    STATE_PATH = Path("/var/lib/ozma/speedtest_state.json")

    def __init__(
        self,
        state_path: Path | None = None,
        event_queue: asyncio.Queue | None = None,
    ) -> None:
        self._state_path = state_path or self.STATE_PATH
        self._event_queue = event_queue
        self._config = SpeedtestConfig()
        self._history: list[SpeedtestResult] = []
        self._running = False
        self._task: asyncio.Task | None = None

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._load()
        self._task = asyncio.create_task(self._run_loop(), name="speedtest.run")
        log.info("speedtest monitor started (enabled=%s)", self._config.enabled)

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None
        log.info("speedtest monitor stopped")

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def get_config(self) -> SpeedtestConfig:
        return self._config

    async def set_config(self, **kwargs: Any) -> SpeedtestConfig:
        for key, value in kwargs.items():
            if not hasattr(self._config, key):
                raise ValueError(f"Unknown config key: {key}")
            setattr(self._config, key, value)
        self._save()
        log.info("speedtest config updated: %s", kwargs)
        return self._config

    def get_status(self) -> dict:
        latest = self.get_latest()
        now = time.time()
        cutoff = now - 86400  # 24 h
        recent = [r for r in self._history if r.timestamp >= cutoff]
        avg_dl = sum(r.download_mbps for r in recent) / len(recent) if recent else 0.0
        avg_ul = sum(r.upload_mbps for r in recent) / len(recent) if recent else 0.0
        avg_ping = sum(r.ping_ms for r in recent) / len(recent) if recent else 0.0
        return {
            "enabled": self._config.enabled,
            "tool": self._config.tool,
            "running": self._running,
            "latest": latest.to_dict() if latest else None,
            "avg_download_mbps": round(avg_dl, 2),
            "avg_upload_mbps": round(avg_ul, 2),
            "avg_ping_ms": round(avg_ping, 2),
            "result_count": len(self._history),
        }

    def get_history(self, limit: int = 48) -> list[dict]:
        return [r.to_dict() for r in self._history[-limit:]]

    def get_latest(self) -> SpeedtestResult | None:
        return self._history[-1] if self._history else None

    async def run_now(self) -> SpeedtestResult | None:
        if self._running:
            log.warning("speedtest already in progress, skipping run_now()")
            return None
        return await self._execute()

    # ------------------------------------------------------------------
    # Internal loop
    # ------------------------------------------------------------------

    async def _run_loop(self) -> None:
        interval_secs = self._config.interval_hours * 3600
        await asyncio.sleep(interval_secs)
        while True:
            if self._config.enabled:
                await self._execute()
            interval_secs = self._config.interval_hours * 3600
            await asyncio.sleep(interval_secs)

    async def _execute(self) -> SpeedtestResult | None:
        """Detect tool, run test, store and return result."""
        self._running = True
        try:
            tool = await self._resolve_tool()
            if tool is None:
                log.warning("no speedtest tool available")
                await self._fire_event({
                    "type": "speedtest.failed",
                    "error": "no speedtest tool available",
                    "ts": time.time(),
                })
                return None

            log.info("running speedtest with %s", tool)
            match tool:
                case "speedtest-cli":
                    result = await self._run_speedtest_cli()
                case "librespeed-cli":
                    result = await self._run_librespeed()
                case "iperf3":
                    result = await self._run_iperf3()
                case _:
                    result = None

            if result is None:
                await self._fire_event({
                    "type": "speedtest.failed",
                    "error": f"{tool} returned no result",
                    "ts": time.time(),
                })
                return None

            self._store_result(result)
            self._check_thresholds(result)
            await self._fire_event({
                "type": "speedtest.completed",
                "download_mbps": result.download_mbps,
                "upload_mbps": result.upload_mbps,
                "ping_ms": result.ping_ms,
                "ts": result.timestamp,
            })
            log.info(
                "speedtest done: dl=%.1f ul=%.1f ping=%.1f ms",
                result.download_mbps, result.upload_mbps, result.ping_ms,
            )
            return result

        except Exception as exc:
            log.exception("speedtest failed: %s", exc)
            await self._fire_event({
                "type": "speedtest.failed",
                "error": str(exc),
                "ts": time.time(),
            })
            return None
        finally:
            self._running = False

    # ------------------------------------------------------------------
    # Tool runners
    # ------------------------------------------------------------------

    async def _run_speedtest_cli(self) -> SpeedtestResult | None:
        proc = await asyncio.create_subprocess_exec(
            "speedtest-cli", "--json",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            log.error("speedtest-cli exited %d: %s", proc.returncode, stderr.decode().strip())
            return None

        try:
            data = json.loads(stdout.decode())
        except json.JSONDecodeError as exc:
            log.error("speedtest-cli JSON parse error: %s", exc)
            return None

        return SpeedtestResult(
            timestamp=time.time(),
            download_mbps=round(data["download"] / 1_000_000, 2),
            upload_mbps=round(data["upload"] / 1_000_000, 2),
            ping_ms=round(data["ping"], 3),
            server_name=data.get("server", {}).get("name", ""),
            server_host=data.get("server", {}).get("host", ""),
            isp=data.get("client", {}).get("isp", ""),
            result_url=data.get("share", ""),
            tool="speedtest-cli",
        )

    async def _run_librespeed(self) -> SpeedtestResult | None:
        proc = await asyncio.create_subprocess_exec(
            "librespeed-cli", "--json", "--simple",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            log.error("librespeed-cli exited %d: %s", proc.returncode, stderr.decode().strip())
            return None

        try:
            data = json.loads(stdout.decode())
            # librespeed returns a list
            entry = data[0] if isinstance(data, list) else data
        except (json.JSONDecodeError, IndexError, KeyError) as exc:
            log.error("librespeed-cli JSON parse error: %s", exc)
            return None

        return SpeedtestResult(
            timestamp=time.time(),
            download_mbps=round(float(entry["download"]), 2),
            upload_mbps=round(float(entry["upload"]), 2),
            ping_ms=round(float(entry["ping"]), 3),
            server_name=entry.get("server", {}).get("name", ""),
            server_host="",
            isp="",
            result_url="",
            tool="librespeed-cli",
        )

    async def _run_iperf3(self) -> SpeedtestResult | None:
        if not self._config.iperf3_server:
            log.error("iperf3 selected but no iperf3_server configured")
            return None

        proc = await asyncio.create_subprocess_exec(
            "iperf3", "-c", self._config.iperf3_server, "-J",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            log.error("iperf3 exited %d: %s", proc.returncode, stderr.decode().strip())
            return None

        try:
            data = json.loads(stdout.decode())
            end = data["end"]
            dl_bps = end["sum_received"]["bits_per_second"]
            ul_bps = end["sum_sent"]["bits_per_second"]
        except (json.JSONDecodeError, KeyError) as exc:
            log.error("iperf3 JSON parse error: %s", exc)
            return None

        # iperf3 doesn't give ping; use 0 as sentinel
        return SpeedtestResult(
            timestamp=time.time(),
            download_mbps=round(dl_bps / 1_000_000, 2),
            upload_mbps=round(ul_bps / 1_000_000, 2),
            ping_ms=0.0,
            server_name=self._config.iperf3_server,
            server_host=self._config.iperf3_server,
            isp="",
            result_url="",
            tool="iperf3",
        )

    # ------------------------------------------------------------------
    # Tool detection
    # ------------------------------------------------------------------

    async def _detect_tool(self) -> str | None:
        candidates: list[str] = ["speedtest-cli", "librespeed-cli"]
        if self._config.iperf3_server:
            candidates.append("iperf3")

        for binary in candidates:
            proc = await asyncio.create_subprocess_exec(
                "which", binary,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.communicate()
            if proc.returncode == 0:
                log.debug("detected speedtest tool: %s", binary)
                return binary

        return None

    async def _resolve_tool(self) -> str | None:
        if self._config.tool == "auto":
            return await self._detect_tool()
        # Explicit tool requested — verify it exists
        proc = await asyncio.create_subprocess_exec(
            "which", self._config.tool,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        await proc.communicate()
        if proc.returncode != 0:
            log.error("configured tool %r not found in PATH", self._config.tool)
            return None
        return self._config.tool

    # ------------------------------------------------------------------
    # Storage and alerting
    # ------------------------------------------------------------------

    def _store_result(self, result: SpeedtestResult) -> None:
        self._history.append(result)
        if len(self._history) > self._config.history_max:
            self._history = self._history[-self._config.history_max:]
        self._save()

    def _check_thresholds(self, result: SpeedtestResult) -> None:
        cfg = self._config
        checks: list[tuple[str, float, float, bool]] = [
            ("download", result.download_mbps, cfg.min_download_mbps, result.download_mbps < cfg.min_download_mbps),
            ("upload",   result.upload_mbps,   cfg.min_upload_mbps,   result.upload_mbps   < cfg.min_upload_mbps),
            ("ping",     result.ping_ms,        cfg.max_ping_ms,       result.ping_ms        > cfg.max_ping_ms),
        ]
        for metric, value, threshold, breached in checks:
            if threshold > 0 and breached:
                log.warning(
                    "speedtest threshold breach: %s=%.2f (threshold=%.2f)",
                    metric, value, threshold,
                )
                asyncio.create_task(
                    self._fire_event({
                        "type": "speedtest.threshold_breach",
                        "metric": metric,
                        "value": value,
                        "threshold": threshold,
                        "ts": time.time(),
                    }),
                    name=f"speedtest.threshold_breach.{metric}",
                )

    async def _fire_event(self, event: dict) -> None:
        if self._event_queue is not None:
            await self._event_queue.put(event)

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        state = {
            "config": self._config.to_dict(),
            "history": [r.to_dict() for r in self._history],
        }
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, indent=2))
        os.chmod(tmp, 0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            raw = json.loads(self._state_path.read_text())
            self._config = SpeedtestConfig.from_dict(raw.get("config", {}))
            self._history = [
                SpeedtestResult.from_dict(r)
                for r in raw.get("history", [])
            ]
            log.debug(
                "loaded speedtest state: %d results, config=%s",
                len(self._history), self._config,
            )
        except Exception as exc:
            log.error("failed to load speedtest state: %s", exc)
