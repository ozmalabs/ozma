# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Arbitrary metrics collection and distribution.

Collects, stores, and serves any key-value metric from any source.
Not limited to hardware sensors — can track anything:

  Hardware:     cpu_temp, gpu_usage, fan_rpm, power_draw
  Application:  stream_bitrate, fps, chat_messages_per_min
  Business:     active_users, api_requests_per_sec, error_rate
  Environment:  room_temp, humidity, noise_level
  Network:      ping_ms, packet_loss, bandwidth_used
  Custom:       anything you POST to the API

Sources:
  - Host agent (polls target machines)
  - Node sensors (INA219, PD, temperature)
  - SNMP (network devices, servers)
  - HTTP pull (any URL returning JSON)
  - MQTT subscribe (IoT sensors)
  - WebSocket push (real-time feeds)
  - REST API push (anything can POST metrics)
  - OCR (read values from captured screens)

Every metric is a namespace.key with a float value, optional unit,
and optional metadata (min, max, warn, crit thresholds).

Storage: ring buffer per metric.  Configurable retention per source
(default 5 min at 1s resolution).  Longer retention at lower resolution
available for dashboard charts.

This replaces AIDA64, Grafana panels, NOC wall dashboards, and
canvas-painter status displays — all rendered by ozma's screen
renderer and displayed on any connected screen.

Video wall:
  Multiple monitors connected to the controller (via capture card
  loopback or Cast devices) can be driven as a synchronised video
  wall.  The renderer generates one large frame and the screen
  manager splits it across displays with pixel-accurate sync.

API:
  GET  /api/v1/metrics                         — all sources, latest
  GET  /api/v1/metrics/{source}                — one source, all metrics
  GET  /api/v1/metrics/{source}/{key}/history  — time series
  POST /api/v1/metrics/{source}                — push arbitrary metrics
  POST /api/v1/metrics/query                   — query multiple metrics
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import urllib.request
from collections import deque
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.metrics")

DEFAULT_HISTORY_SIZE = 300    # 5 min at 1s
LONG_HISTORY_SIZE = 3600      # 1 hour at 1s
POLL_INTERVAL = 2.0


@dataclass
class MetricDefinition:
    """Definition of a metric with optional thresholds and formatting."""
    key: str
    unit: str = ""              # "%", "°C", "RPM", "bytes/s", "ms", custom
    min_val: float | None = None
    max_val: float | None = None
    warn_threshold: float | None = None   # Yellow above this
    crit_threshold: float | None = None   # Red above this
    format: str = ""            # "percent", "bytes", "duration", "number", custom
    label: str = ""             # Human-readable label
    group: str = ""             # Grouping: "cpu", "gpu", "disk", "network", "custom"


@dataclass
class MetricSource:
    """A source of metrics (a machine, a service, a sensor, anything)."""

    id: str                     # Unique source ID (node_id, service name, etc.)
    name: str = ""              # Human-readable name
    source_type: str = ""       # "node", "agent", "snmp", "http", "mqtt", "push", "manual"
    metrics: dict[str, float] = field(default_factory=dict)
    definitions: dict[str, MetricDefinition] = field(default_factory=dict)
    history: dict[str, deque] = field(default_factory=dict)
    tags: dict[str, str] = field(default_factory=dict)  # Arbitrary metadata
    last_updated: float = 0.0
    online: bool = True

    def update(self, key: str, value: float, definition: MetricDefinition | None = None) -> None:
        self.metrics[key] = value
        if definition:
            self.definitions[key] = definition
        if key not in self.history:
            self.history[key] = deque(maxlen=DEFAULT_HISTORY_SIZE)
        self.history[key].append((time.time(), value))
        self.last_updated = time.time()

    def update_batch(self, data: dict[str, float]) -> None:
        for key, value in data.items():
            if isinstance(value, (int, float)):
                self.update(key, float(value))

    def get_state(self, key: str) -> str:
        """Return "ok", "warn", or "crit" based on thresholds."""
        val = self.metrics.get(key)
        defn = self.definitions.get(key)
        if val is None or not defn:
            return "ok"
        if defn.crit_threshold is not None and val >= defn.crit_threshold:
            return "crit"
        if defn.warn_threshold is not None and val >= defn.warn_threshold:
            return "warn"
        return "ok"

    def to_dict(self, include_history: bool = False) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id,
            "name": self.name,
            "source_type": self.source_type,
            "tags": self.tags,
            "online": self.online,
            "age_s": round(time.time() - self.last_updated, 1) if self.last_updated else None,
            "metrics": {},
        }
        for key, val in self.metrics.items():
            defn = self.definitions.get(key)
            entry: dict[str, Any] = {"value": val, "state": self.get_state(key)}
            if defn:
                if defn.unit: entry["unit"] = defn.unit
                if defn.label: entry["label"] = defn.label
                if defn.group: entry["group"] = defn.group
                if defn.format: entry["format"] = defn.format
                if defn.warn_threshold is not None: entry["warn"] = defn.warn_threshold
                if defn.crit_threshold is not None: entry["crit"] = defn.crit_threshold
            d["metrics"][key] = entry

        if include_history:
            d["history"] = {
                key: list(samples) for key, samples in self.history.items()
            }
        return d


# ── Default metric definitions (sensible defaults for common hardware) ───────

_HARDWARE_DEFAULTS: dict[str, MetricDefinition] = {
    "cpu_usage": MetricDefinition("cpu_usage", unit="%", max_val=100, warn_threshold=85, crit_threshold=95, format="percent", label="CPU Usage", group="cpu"),
    "cpu_temp": MetricDefinition("cpu_temp", unit="°C", warn_threshold=80, crit_threshold=95, label="CPU Temp", group="cpu"),
    "gpu_usage": MetricDefinition("gpu_usage", unit="%", max_val=100, warn_threshold=90, crit_threshold=98, format="percent", label="GPU Usage", group="gpu"),
    "gpu_temp": MetricDefinition("gpu_temp", unit="°C", warn_threshold=85, crit_threshold=95, label="GPU Temp", group="gpu"),
    "ram_used": MetricDefinition("ram_used", unit="bytes", format="bytes", label="RAM Used", group="memory"),
    "ram_total": MetricDefinition("ram_total", unit="bytes", format="bytes", label="RAM Total", group="memory"),
    "disk_used": MetricDefinition("disk_used", unit="bytes", format="bytes", label="Disk Used", group="disk"),
    "disk_total": MetricDefinition("disk_total", unit="bytes", format="bytes", label="Disk Total", group="disk"),
    "disk_read_rate": MetricDefinition("disk_read_rate", unit="bytes/s", format="bytes", label="Disk Read", group="disk"),
    "disk_write_rate": MetricDefinition("disk_write_rate", unit="bytes/s", format="bytes", label="Disk Write", group="disk"),
    "net_rx_rate": MetricDefinition("net_rx_rate", unit="bytes/s", format="bytes", label="Net ↓", group="network"),
    "net_tx_rate": MetricDefinition("net_tx_rate", unit="bytes/s", format="bytes", label="Net ↑", group="network"),
    "fan_rpm": MetricDefinition("fan_rpm", unit="RPM", label="Fan Speed", group="cooling"),
    "power_draw": MetricDefinition("power_draw", unit="W", warn_threshold=200, crit_threshold=300, label="Power", group="power"),
    "uptime": MetricDefinition("uptime", unit="s", format="duration", label="Uptime", group="system"),
}


class MetricsCollector:
    """
    Arbitrary metrics collection and distribution.

    Accepts metrics from any source — hardware agents, SNMP, HTTP pull,
    MQTT subscribe, manual push.  Every metric has a namespace (source),
    key, value, and optional definition with thresholds.
    """

    def __init__(self, state: Any) -> None:
        self._state = state
        self._sources: dict[str, MetricSource] = {}
        self._http_pull_configs: list[dict] = []  # URL → source_id mapping
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._collect_loop(), name="metrics-collector")
        log.info("Metrics collector started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    # ── Public API ───────────────────────────────────────────────────────────

    def get_all(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._sources.values()]

    def get_device(self, source_id: str, include_history: bool = False) -> dict[str, Any] | None:
        s = self._sources.get(source_id)
        return s.to_dict(include_history=include_history) if s else None

    def get_metric(self, source_id: str, key: str) -> float | None:
        s = self._sources.get(source_id)
        return s.metrics.get(key) if s else None

    def get_history(self, source_id: str, key: str) -> list[tuple[float, float]]:
        s = self._sources.get(source_id)
        if s and key in s.history:
            return list(s.history[key])
        return []

    def list_sources(self) -> list[str]:
        return list(self._sources.keys())

    # ── Push API (anything can send metrics) ─────────────────────────────────

    def push(self, source_id: str, metrics: dict[str, float],
             name: str = "", tags: dict[str, str] | None = None,
             definitions: dict[str, dict] | None = None) -> None:
        """Accept arbitrary metrics from any source."""
        if source_id not in self._sources:
            self._sources[source_id] = MetricSource(
                id=source_id, name=name or source_id, source_type="push",
                tags=tags or {},
            )
        src = self._sources[source_id]
        if tags:
            src.tags.update(tags)

        # Apply definitions if provided
        if definitions:
            for key, defn_dict in definitions.items():
                src.definitions[key] = MetricDefinition(key=key, **defn_dict)

        # Apply default definitions for known metric names
        for key in metrics:
            if key in _HARDWARE_DEFAULTS and key not in src.definitions:
                src.definitions[key] = _HARDWARE_DEFAULTS[key]

        src.update_batch(metrics)

    def define_metric(self, source_id: str, key: str, **kwargs: Any) -> None:
        """Define or update a metric's metadata (unit, thresholds, label)."""
        if source_id not in self._sources:
            self._sources[source_id] = MetricSource(id=source_id, name=source_id)
        self._sources[source_id].definitions[key] = MetricDefinition(key=key, **kwargs)

    # ── HTTP pull sources (poll external URLs) ───────────────────────────────

    def add_http_source(self, source_id: str, url: str, interval_s: float = 5.0,
                         name: str = "", tags: dict[str, str] | None = None) -> None:
        """Add an HTTP URL to poll for metrics (JSON response → flat key:value)."""
        self._http_pull_configs.append({
            "source_id": source_id, "url": url,
            "interval_s": interval_s, "name": name, "tags": tags or {},
        })
        if source_id not in self._sources:
            self._sources[source_id] = MetricSource(
                id=source_id, name=name or source_id,
                source_type="http", tags=tags or {},
            )

    # ── Query API (for complex dashboards) ───────────────────────────────────

    def query(self, queries: list[dict]) -> list[dict]:
        """
        Query multiple metrics at once.

        Each query: {"source": "node-1", "key": "cpu_temp"}
        or: {"source": "*", "key": "cpu_temp"} for all sources.

        Returns: [{"source": "node-1", "key": "cpu_temp", "value": 65.2, "state": "ok"}, ...]
        """
        results = []
        for q in queries:
            src_pattern = q.get("source", "*")
            key = q.get("key", "")

            sources = self._sources.values() if src_pattern == "*" else \
                      [self._sources[src_pattern]] if src_pattern in self._sources else []

            for src in sources:
                if key in src.metrics:
                    results.append({
                        "source": src.id,
                        "source_name": src.name,
                        "key": key,
                        "value": src.metrics[key],
                        "state": src.get_state(key),
                        "unit": src.definitions.get(key, MetricDefinition(key)).unit,
                    })
                elif key == "*":
                    for k, v in src.metrics.items():
                        results.append({
                            "source": src.id, "source_name": src.name,
                            "key": k, "value": v, "state": src.get_state(k),
                        })
        return results

    # ── Collection loop ──────────────────────────────────────────────────────

    async def _collect_loop(self) -> None:
        while True:
            try:
                # Collect from ozma nodes
                for node in list(self._state.nodes.values()):
                    await self._collect_node(node)

                # Collect from HTTP pull sources
                for cfg in self._http_pull_configs:
                    await self._collect_http(cfg)

                await asyncio.sleep(POLL_INTERVAL)
            except asyncio.CancelledError:
                return

    async def _collect_node(self, node: Any) -> None:
        node_id = node.id
        if node_id not in self._sources:
            self._sources[node_id] = MetricSource(
                id=node_id, name=node_id.split(".")[0], source_type="node",
            )
        src = self._sources[node_id]

        # Try host agent
        if await self._fetch_agent(node, src):
            src.source_type = "agent"
            return

        # Fall back to node sensors
        await self._fetch_node_sensors(node, src)

    async def _fetch_agent(self, node: Any, src: MetricSource) -> bool:
        if not node.api_port:
            return False
        try:
            loop = asyncio.get_running_loop()
            url = f"http://{node.host}:{node.api_port}/agent/metrics"
            def _f():
                with urllib.request.urlopen(url, timeout=2) as r:
                    return json.loads(r.read())
            data = await loop.run_in_executor(None, _f)
            src.update_batch(data)
            # Apply default definitions
            for key in data:
                if key in _HARDWARE_DEFAULTS and key not in src.definitions:
                    src.definitions[key] = _HARDWARE_DEFAULTS[key]
            src.online = True
            return True
        except Exception:
            return False

    async def _fetch_node_sensors(self, node: Any, src: MetricSource) -> None:
        if not node.api_port:
            return
        loop = asyncio.get_running_loop()
        for endpoint, mapping in [
            ("/current", {"mA": "usb_current_ma", "mW": "power_mw", "V": "usb_voltage_v"}),
            ("/pd/state", {"negotiated_voltage": "pd_voltage", "negotiated_current": "pd_current"}),
        ]:
            try:
                url = f"http://{node.host}:{node.api_port}{endpoint}"
                def _f(u=url):
                    with urllib.request.urlopen(u, timeout=2) as r:
                        return json.loads(r.read())
                data = await loop.run_in_executor(None, _f)
                for src_key, dst_key in mapping.items():
                    if src_key in data and isinstance(data[src_key], (int, float)):
                        src.update(dst_key, float(data[src_key]))
                src.online = True
            except Exception:
                pass

    async def _collect_http(self, cfg: dict) -> None:
        src = self._sources.get(cfg["source_id"])
        if not src:
            return
        try:
            loop = asyncio.get_running_loop()
            url = cfg["url"]
            def _f():
                with urllib.request.urlopen(url, timeout=5) as r:
                    return json.loads(r.read())
            data = await loop.run_in_executor(None, _f)
            # Flatten nested JSON into dot-notation keys
            flat = _flatten(data)
            src.update_batch(flat)
            src.online = True
        except Exception:
            src.online = False


def _flatten(d: dict, prefix: str = "") -> dict[str, float]:
    """Flatten a nested dict to dot-notation keys, keeping only numeric values."""
    result = {}
    for k, v in d.items():
        key = f"{prefix}{k}" if not prefix else f"{prefix}.{k}"
        if isinstance(v, (int, float)):
            result[key] = float(v)
        elif isinstance(v, dict):
            result.update(_flatten(v, key))
    return result
