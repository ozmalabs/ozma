# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
UPS / power management monitor using NUT (Network UPS Tools).

Polls upsc at a configurable interval, fires WebSocket events on state
transitions, and can initiate a graceful system shutdown when battery is
critically low.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.ups_monitor")


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class UPSConfig:
    enabled: bool = False
    nut_host: str = "localhost"
    nut_port: int = 3493
    ups_name: str = "ups"
    poll_interval_seconds: int = 30
    # Alert thresholds
    battery_warn_pct: int = 50
    battery_critical_pct: int = 25
    battery_shutdown_pct: int = 10
    runtime_warn_minutes: int = 10
    auto_shutdown: bool = True

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "UPSConfig":
        known = {f for f in cls.__dataclass_fields__}
        return cls(**{k: v for k, v in data.items() if k in known})


@dataclass
class UPSStatus:
    ups_name: str
    model: str = ""
    status: str = ""
    on_battery: bool = False
    battery_pct: float = 100.0
    battery_voltage: float = 0.0
    runtime_seconds: int = 0
    load_pct: float = 0.0
    input_voltage: float = 0.0
    output_voltage: float = 0.0
    temperature: float | None = None
    last_polled: float = field(default_factory=time.time)
    reachable: bool = False

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class UPSMonitor:
    STATE_PATH = Path("/var/lib/ozma/ups_state.json")

    def __init__(
        self,
        state_path: Path | None = None,
        event_queue: asyncio.Queue | None = None,
        alert_callback=None,
    ) -> None:
        self._state_path = state_path or self.STATE_PATH
        self._event_queue = event_queue
        self._alert_callback = alert_callback  # async callable(level, message)

        self._config = UPSConfig()
        self._status: UPSStatus | None = None

        # Transition tracking
        self._prev_on_battery: bool = False
        self._prev_reachable: bool = True   # assume reachable until first poll
        self._first_poll: bool = True        # suppress "restored" on startup
        # None | "warn" | "critical" | "shutdown"
        self._last_alert_level: str | None = None
        self._shutdown_initiated: bool = False

        self._poll_task: asyncio.Task | None = None
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        if not self._config.enabled:
            log.info("UPS monitor disabled — not starting")
            return
        log.info(
            "UPS monitor starting (host=%s:%s ups=%s interval=%ss)",
            self._config.nut_host,
            self._config.nut_port,
            self._config.ups_name,
            self._config.poll_interval_seconds,
        )
        self._poll_task = asyncio.create_task(self._poll_loop(), name="ups.poll")

    async def stop(self) -> None:
        if self._poll_task and not self._poll_task.done():
            self._poll_task.cancel()
            try:
                await self._poll_task
            except asyncio.CancelledError:
                pass
        self._poll_task = None
        log.info("UPS monitor stopped")

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def get_config(self) -> UPSConfig:
        return self._config

    async def set_config(self, **kwargs) -> UPSConfig:
        for key, value in kwargs.items():
            if not hasattr(self._config, key):
                raise ValueError(f"Unknown UPSConfig field: {key!r}")
            setattr(self._config, key, value)
        self._save()

        # Restart poll loop if enabled state or interval changed
        was_running = self._poll_task and not self._poll_task.done()
        if was_running:
            await self.stop()
        if self._config.enabled:
            await self.start()

        return self._config

    def get_status(self) -> dict[str, Any]:
        base = self._status.to_dict() if self._status else {}
        base["config"] = self._config.to_dict()
        return base

    # ------------------------------------------------------------------
    # Polling
    # ------------------------------------------------------------------

    async def poll_now(self) -> UPSStatus | None:
        cfg = self._config
        target = f"{cfg.ups_name}@{cfg.nut_host}:{cfg.nut_port}"
        try:
            proc = await asyncio.create_subprocess_exec(
                "upsc",
                target,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=10.0)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.communicate()
                log.warning("upsc timed out for %s", target)
                return self._make_unreachable_status()

            if proc.returncode != 0:
                log.warning(
                    "upsc returned %s for %s: %s",
                    proc.returncode,
                    target,
                    stderr.decode(errors="replace").strip(),
                )
                return self._make_unreachable_status()

        except FileNotFoundError:
            log.warning("upsc not found — is nut-client installed?")
            return self._make_unreachable_status()

        raw = self._parse_upsc_output(stdout.decode(errors="replace"))
        return self._build_status(raw)

    def _make_unreachable_status(self) -> UPSStatus:
        return UPSStatus(
            ups_name=self._config.ups_name,
            reachable=False,
            last_polled=time.time(),
        )

    def _parse_upsc_output(self, output: str) -> dict[str, str]:
        result: dict[str, str] = {}
        for line in output.splitlines():
            if ": " in line:
                key, _, value = line.partition(": ")
                result[key.strip()] = value.strip()
        return result

    def _build_status(self, raw: dict[str, str]) -> UPSStatus:
        cfg = self._config

        def _float(key: str, default: float = 0.0) -> float:
            try:
                return float(raw.get(key, default))
            except (ValueError, TypeError):
                return default

        def _int(key: str, default: int = 0) -> int:
            try:
                return int(float(raw.get(key, default)))
            except (ValueError, TypeError):
                return default

        status_str = raw.get("ups.status", "")
        on_battery = "OB" in status_str

        temp_raw = raw.get("ups.temperature") or raw.get("battery.temperature")
        temperature: float | None = None
        if temp_raw is not None:
            try:
                temperature = float(temp_raw)
            except (ValueError, TypeError):
                pass

        return UPSStatus(
            ups_name=cfg.ups_name,
            model=raw.get("device.model", raw.get("ups.model", "")),
            status=status_str,
            on_battery=on_battery,
            battery_pct=_float("battery.charge", 100.0),
            battery_voltage=_float("battery.voltage"),
            runtime_seconds=_int("battery.runtime"),
            load_pct=_float("ups.load"),
            input_voltage=_float("input.voltage"),
            output_voltage=_float("output.voltage"),
            temperature=temperature,
            last_polled=time.time(),
            reachable=True,
        )

    # ------------------------------------------------------------------
    # Poll loop
    # ------------------------------------------------------------------

    async def _poll_loop(self) -> None:
        while True:
            try:
                status = await self.poll_now()
                if status is not None:
                    self._status = status
                    self._check_thresholds(status)
                    self._first_poll = False
            except Exception:
                log.exception("Unexpected error in UPS poll loop")

            await asyncio.sleep(self._config.poll_interval_seconds)

    # ------------------------------------------------------------------
    # Threshold checking and event firing
    # ------------------------------------------------------------------

    def _check_thresholds(self, status: UPSStatus) -> None:
        cfg = self._config
        ts = status.last_polled

        # Reachability transitions
        if not status.reachable:
            if self._prev_reachable:
                log.warning("UPS unreachable at %s:%s", cfg.nut_host, cfg.nut_port)
                asyncio.create_task(
                    self._fire_event({"type": "ups.unreachable", "ts": ts}),
                    name="ups.event.unreachable",
                )
                if self._alert_callback:
                    asyncio.create_task(
                        self._alert_callback("warn", "UPS unreachable — check NUT daemon"),
                        name="ups.alert.unreachable",
                    )
                self._prev_reachable = False
            return

        # Device became reachable again
        if not self._prev_reachable:
            log.info("UPS reachable again")
            self._prev_reachable = True

        # AC restored (was on battery, now back on line)
        if not self._first_poll and self._prev_on_battery and not status.on_battery:
            log.info("AC power restored — battery at %.1f%%", status.battery_pct)
            asyncio.create_task(
                self._fire_event({
                    "type": "ups.restored",
                    "battery_pct": status.battery_pct,
                    "ts": ts,
                }),
                name="ups.event.restored",
            )
            if self._alert_callback:
                asyncio.create_task(
                    self._alert_callback(
                        "info",
                        f"AC power restored — battery at {status.battery_pct:.0f}%",
                    ),
                    name="ups.alert.restored",
                )
            self._last_alert_level = None
            self._shutdown_initiated = False

        # Switched to battery
        if not self._first_poll and not self._prev_on_battery and status.on_battery:
            log.warning(
                "On battery power — %.1f%% charge, %ds runtime",
                status.battery_pct,
                status.runtime_seconds,
            )
            asyncio.create_task(
                self._fire_event({
                    "type": "ups.on_battery",
                    "battery_pct": status.battery_pct,
                    "runtime_seconds": status.runtime_seconds,
                    "ts": ts,
                }),
                name="ups.event.on_battery",
            )
            if self._alert_callback:
                runtime_min = status.runtime_seconds // 60
                asyncio.create_task(
                    self._alert_callback(
                        "warn",
                        f"On battery — {status.battery_pct:.0f}% charge, ~{runtime_min}min runtime",
                    ),
                    name="ups.alert.on_battery",
                )

        self._prev_on_battery = status.on_battery

        if not status.on_battery:
            # Nothing more to check when on AC
            return

        # Battery level alerts (only escalate, reset when AC restored)
        batt = status.battery_pct

        if batt <= cfg.battery_shutdown_pct:
            if self._last_alert_level != "shutdown":
                log.error(
                    "Battery critical (%.1f%% <= %d%%) — shutdown threshold reached",
                    batt,
                    cfg.battery_shutdown_pct,
                )
                asyncio.create_task(
                    self._fire_event({
                        "type": "ups.battery_critical",
                        "battery_pct": batt,
                        "ts": ts,
                    }),
                    name="ups.event.battery_critical",
                )
                if self._alert_callback:
                    asyncio.create_task(
                        self._alert_callback(
                            "critical",
                            f"UPS battery critical at {batt:.0f}% — shutdown imminent",
                        ),
                        name="ups.alert.battery_critical",
                    )
                self._last_alert_level = "shutdown"

            if cfg.auto_shutdown and not self._shutdown_initiated:
                self._shutdown_initiated = True
                asyncio.create_task(self._shutdown(), name="ups.shutdown")

        elif batt <= cfg.battery_critical_pct:
            if self._last_alert_level not in ("critical", "shutdown"):
                log.warning(
                    "Battery critical-level (%.1f%% <= %d%%)",
                    batt,
                    cfg.battery_critical_pct,
                )
                asyncio.create_task(
                    self._fire_event({
                        "type": "ups.battery_critical",
                        "battery_pct": batt,
                        "threshold_pct": cfg.battery_critical_pct,
                        "ts": ts,
                    }),
                    name="ups.event.battery_critical",
                )
                if self._alert_callback:
                    asyncio.create_task(
                        self._alert_callback(
                            "error",
                            f"UPS battery at {batt:.0f}% — running low",
                        ),
                        name="ups.alert.battery_critical",
                    )
                self._last_alert_level = "critical"

        elif batt <= cfg.battery_warn_pct:
            if self._last_alert_level not in ("warn", "critical", "shutdown"):
                log.warning(
                    "Battery low (%.1f%% <= %d%%)",
                    batt,
                    cfg.battery_warn_pct,
                )
                runtime_min = status.runtime_seconds // 60
                asyncio.create_task(
                    self._fire_event({
                        "type": "ups.battery_low",
                        "battery_pct": batt,
                        "threshold_pct": cfg.battery_warn_pct,
                        "runtime_minutes": runtime_min,
                        "ts": ts,
                    }),
                    name="ups.event.battery_low",
                )
                if self._alert_callback:
                    asyncio.create_task(
                        self._alert_callback(
                            "warn",
                            f"UPS battery at {batt:.0f}% (~{runtime_min}min remaining)",
                        ),
                        name="ups.alert.battery_low",
                    )
                self._last_alert_level = "warn"

        # Runtime warning (independent of battery %)
        runtime_min = status.runtime_seconds / 60
        if runtime_min < cfg.runtime_warn_minutes and self._last_alert_level is None:
            log.warning(
                "UPS runtime low: %d minutes remaining",
                int(runtime_min),
            )
            asyncio.create_task(
                self._fire_event({
                    "type": "ups.battery_low",
                    "battery_pct": batt,
                    "threshold_pct": cfg.battery_warn_pct,
                    "runtime_minutes": int(runtime_min),
                    "ts": ts,
                }),
                name="ups.event.runtime_low",
            )

    # ------------------------------------------------------------------
    # Shutdown
    # ------------------------------------------------------------------

    async def _shutdown(self) -> None:
        batt = self._status.battery_pct if self._status else 0.0
        reason = f"UPS battery critical ({batt:.0f}%) — shutting down"
        log.warning("Initiating system shutdown: %s", reason)

        asyncio.create_task(
            self._fire_event({
                "type": "ups.shutdown_initiated",
                "battery_pct": batt,
                "reason": reason,
                "ts": time.time(),
            }),
            name="ups.event.shutdown_initiated",
        )

        try:
            proc = await asyncio.create_subprocess_exec(
                "shutdown",
                "-h",
                "+2",
                reason,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await proc.communicate()
            if proc.returncode != 0:
                log.error("shutdown command returned %s", proc.returncode)
        except FileNotFoundError:
            log.error("shutdown command not found — cannot initiate system shutdown")

    # ------------------------------------------------------------------
    # Events
    # ------------------------------------------------------------------

    async def _fire_event(self, event: dict) -> None:
        if self._event_queue is not None:
            await self._event_queue.put(event)

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        data = {"config": self._config.to_dict()}
        tmp.write_text(json.dumps(data, indent=2))
        os.chmod(tmp, 0o600)
        tmp.rename(self._state_path)
        log.debug("UPS config saved to %s", self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            if "config" in data:
                self._config = UPSConfig.from_dict(data["config"])
            log.debug("UPS config loaded from %s", self._state_path)
        except Exception:
            log.exception("Failed to load UPS state from %s", self._state_path)
