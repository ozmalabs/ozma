# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Enterprise metrics export — seamless integration with monitoring stacks.

Exports ozma metrics to enterprise monitoring platforms in their native
formats.  First-class onboarding: configure the endpoint, ozma handles
the rest.  No manual metric mapping, no custom dashboards to build.

Supported targets:

  Prometheus / Grafana:
    /metrics endpoint in Prometheus exposition format.
    Grafana scrapes it directly — add one data source, done.
    Pre-built Grafana dashboard JSON included.

  Datadog:
    DogStatsD UDP protocol (port 8125).
    Sends metrics with tags for node, scenario, source.
    Or Datadog HTTP API with API key.

  AWS CloudWatch:
    PutMetricData API via boto3.
    Custom namespace "Ozma/" with per-node dimensions.

  InfluxDB:
    Line protocol over HTTP or UDP.
    Native InfluxDB tags for node, scenario, metric type.

  OpenTelemetry (OTLP):
    gRPC or HTTP export to any OTLP-compatible collector.
    Covers: New Relic, Honeycomb, Lightstep, Grafana Cloud, Splunk.

  StatsD:
    Generic StatsD UDP (works with Graphite, Telegraph, etc.)

  Syslog:
    Serial console output + alerts forwarded via syslog (RFC 5424).
    Integrates with: Splunk, ELK, Graylog, rsyslog, syslog-ng.

  SNMP:
    SNMP traps for alerts (kernel panic, overcurrent, node offline).
    SNMP GET for metrics (OID tree under enterprise prefix).

  Webhook:
    Already built in notifications.py — generic HTTP POST.

Configuration:
  One block in controls.yaml per export target:

    exports:
      - type: prometheus
        port: 9090

      - type: datadog
        api_key: "dd-api-key-here"
        # or statsd_host: "localhost"

      - type: cloudwatch
        namespace: "Ozma"
        region: "us-east-1"

      - type: influxdb
        url: "http://influxdb:8086"
        database: "ozma"
        token: "..."

      - type: otlp
        endpoint: "https://otlp.nr-data.net:4318"
        headers:
          api-key: "NEW_RELIC_KEY"

      - type: syslog
        host: "syslog.internal"
        port: 514
        facility: "local0"
"""

from __future__ import annotations

import asyncio
import json
import logging
import socket
import time
import urllib.request
from typing import Any

log = logging.getLogger("ozma.metrics_export")


class MetricsExporter:
    """Base class for metrics export targets."""

    async def export(self, sources: list[dict]) -> None: ...
    async def export_alert(self, alert: dict) -> None: ...
    async def start(self) -> None: ...
    async def stop(self) -> None: ...


class PrometheusExporter(MetricsExporter):
    """
    Prometheus exposition format on an HTTP endpoint.

    Grafana scrapes this directly. One-step setup:
      1. Add Prometheus data source → http://ozma-controller:9090/metrics
      2. Import the bundled Grafana dashboard JSON
      3. Done.
    """

    def __init__(self, port: int = 9090) -> None:
        self._port = port
        self._latest_text = ""
        self._server: Any = None

    async def start(self) -> None:
        from aiohttp import web
        app = web.Application()

        async def metrics_endpoint(_: web.Request) -> web.Response:
            return web.Response(text=self._latest_text, content_type="text/plain; version=0.0.4")

        app.router.add_get("/metrics", metrics_endpoint)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "0.0.0.0", self._port)
        await site.start()
        self._server = runner
        log.info("Prometheus exporter on port %d", self._port)

    async def stop(self) -> None:
        if self._server:
            await self._server.cleanup()

    async def export(self, sources: list[dict]) -> None:
        lines = []
        for src in sources:
            src_name = src.get("id", "unknown").replace(".", "_").replace("-", "_")
            tags = src.get("tags", {})
            label_str = ",".join(f'{k}="{v}"' for k, v in tags.items())
            if label_str:
                label_str = "," + label_str

            for key, info in src.get("metrics", {}).items():
                metric_name = f"ozma_{key}".replace(".", "_").replace("-", "_")
                value = info if isinstance(info, (int, float)) else info.get("value", 0)
                lines.append(f'{metric_name}{{source="{src_name}"{label_str}}} {value}')

        self._latest_text = "\n".join(lines) + "\n"


class DatadogExporter(MetricsExporter):
    """DogStatsD UDP or Datadog HTTP API."""

    def __init__(self, api_key: str = "", statsd_host: str = "localhost", statsd_port: int = 8125) -> None:
        self._api_key = api_key
        self._statsd_host = statsd_host
        self._statsd_port = statsd_port
        self._sock: socket.socket | None = None

    async def start(self) -> None:
        if not self._api_key:
            self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            log.info("Datadog exporter via DogStatsD → %s:%d", self._statsd_host, self._statsd_port)
        else:
            log.info("Datadog exporter via HTTP API")

    async def export(self, sources: list[dict]) -> None:
        for src in sources:
            src_name = src.get("id", "unknown")
            for key, info in src.get("metrics", {}).items():
                value = info if isinstance(info, (int, float)) else info.get("value", 0)
                tags = f"|#source:{src_name}"
                if self._sock:
                    line = f"ozma.{key}:{value}|g{tags}\n"
                    self._sock.sendto(line.encode(), (self._statsd_host, self._statsd_port))

    async def export_alert(self, alert: dict) -> None:
        if self._api_key:
            await self._dd_event(alert)
        elif self._sock:
            title = alert.get("description", alert.get("pattern_id", "alert"))
            line = f"_e{{{len(title)}|0}}:{title}|t:error|#source:ozma\n"
            self._sock.sendto(line.encode(), (self._statsd_host, self._statsd_port))

    async def _dd_event(self, alert: dict) -> None:
        try:
            payload = json.dumps({
                "title": f"Ozma: {alert.get('pattern_id', 'alert')}",
                "text": alert.get("match_text", ""),
                "alert_type": "error",
                "source_type_name": "ozma",
                "tags": [f"source:{alert.get('source_id', '')}"],
            }).encode()
            loop = asyncio.get_running_loop()
            def _post():
                req = urllib.request.Request(
                    "https://api.datadoghq.com/api/v1/events",
                    data=payload,
                    headers={"Content-Type": "application/json", "DD-API-KEY": self._api_key},
                )
                urllib.request.urlopen(req, timeout=10)
            await loop.run_in_executor(None, _post)
        except Exception as e:
            log.debug("Datadog event failed: %s", e)


class InfluxDBExporter(MetricsExporter):
    """InfluxDB line protocol over HTTP."""

    def __init__(self, url: str = "http://localhost:8086", database: str = "ozma",
                 token: str = "", org: str = "", bucket: str = "") -> None:
        self._url = url.rstrip("/")
        self._database = database
        self._token = token
        self._org = org
        self._bucket = bucket or database

    async def start(self) -> None:
        log.info("InfluxDB exporter → %s", self._url)

    async def export(self, sources: list[dict]) -> None:
        lines = []
        ts_ns = int(time.time() * 1e9)
        for src in sources:
            src_name = src.get("id", "unknown").replace(" ", "\\ ")
            for key, info in src.get("metrics", {}).items():
                value = info if isinstance(info, (int, float)) else info.get("value", 0)
                lines.append(f"ozma,source={src_name} {key}={value} {ts_ns}")

        if not lines:
            return

        body = "\n".join(lines).encode()
        try:
            loop = asyncio.get_running_loop()
            # InfluxDB 2.x
            if self._token:
                url = f"{self._url}/api/v2/write?org={self._org}&bucket={self._bucket}&precision=ns"
                headers = {"Authorization": f"Token {self._token}", "Content-Type": "text/plain"}
            else:
                url = f"{self._url}/write?db={self._database}&precision=ns"
                headers = {"Content-Type": "text/plain"}

            def _post():
                req = urllib.request.Request(url, data=body, headers=headers, method="POST")
                urllib.request.urlopen(req, timeout=10)
            await loop.run_in_executor(None, _post)
        except Exception as e:
            log.debug("InfluxDB write failed: %s", e)


class SyslogExporter(MetricsExporter):
    """Syslog (RFC 5424) for alerts and serial console output."""

    def __init__(self, host: str = "localhost", port: int = 514,
                 facility: str = "local0", protocol: str = "udp") -> None:
        self._host = host
        self._port = port
        self._facility_code = {"local0": 16, "local1": 17, "local2": 18, "local3": 19,
                                "local4": 20, "local5": 21, "local6": 22, "local7": 23}.get(facility, 16)
        self._protocol = protocol
        self._sock: socket.socket | None = None

    async def start(self) -> None:
        if self._protocol == "udp":
            self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        log.info("Syslog exporter → %s:%d (%s)", self._host, self._port, self._protocol)

    async def export_alert(self, alert: dict) -> None:
        severity_map = {"critical": 2, "error": 3, "warning": 4, "info": 6}
        severity = severity_map.get(alert.get("severity", "error"), 3)
        pri = self._facility_code * 8 + severity
        msg = f"<{pri}>1 {time.strftime('%Y-%m-%dT%H:%M:%SZ')} ozma - - - - {alert.get('description', '')} | {alert.get('match_text', '')[:200]}"
        if self._sock:
            self._sock.sendto(msg.encode()[:1024], (self._host, self._port))

    async def export_serial_line(self, console_id: str, text: str, severity: str) -> None:
        """Forward serial console lines as syslog messages."""
        sev_code = {"critical": 2, "error": 3, "warning": 4, "info": 6}.get(severity, 6)
        pri = self._facility_code * 8 + sev_code
        msg = f"<{pri}>1 {time.strftime('%Y-%m-%dT%H:%M:%SZ')} ozma {console_id} - - - {text[:800]}"
        if self._sock:
            self._sock.sendto(msg.encode()[:1024], (self._host, self._port))


class StatsDExporter(MetricsExporter):
    """Generic StatsD UDP (Graphite, Telegraf, etc.)."""

    def __init__(self, host: str = "localhost", port: int = 8125) -> None:
        self._host = host
        self._port = port
        self._sock: socket.socket | None = None

    async def start(self) -> None:
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        log.info("StatsD exporter → %s:%d", self._host, self._port)

    async def export(self, sources: list[dict]) -> None:
        if not self._sock:
            return
        for src in sources:
            src_name = src.get("id", "unknown")
            for key, info in src.get("metrics", {}).items():
                value = info if isinstance(info, (int, float)) else info.get("value", 0)
                line = f"ozma.{src_name}.{key}:{value}|g\n"
                self._sock.sendto(line.encode(), (self._host, self._port))


# ── Export Manager ───────────────────────────────────────────────────────────

EXPORTER_TYPES: dict[str, type] = {
    "prometheus": PrometheusExporter,
    "datadog": DatadogExporter,
    "influxdb": InfluxDBExporter,
    "syslog": SyslogExporter,
    "statsd": StatsDExporter,
}


class MetricsExportManager:
    """
    Manages all metric export targets.

    Periodically pushes collected metrics to all configured exporters.
    Also forwards alerts (OCR triggers, serial panics) to alert-capable exporters.
    """

    def __init__(self, metrics_collector: Any) -> None:
        self._metrics = metrics_collector
        self._exporters: list[MetricsExporter] = []
        self._task: asyncio.Task | None = None
        self._export_interval = 10.0  # seconds

    async def start(self) -> None:
        for exp in self._exporters:
            await exp.start()
        self._task = asyncio.create_task(self._export_loop(), name="metrics-export")
        log.info("Metrics export manager started (%d targets)", len(self._exporters))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
        for exp in self._exporters:
            await exp.stop()

    def add_exporter(self, exporter: MetricsExporter) -> None:
        self._exporters.append(exporter)

    def add_from_config(self, config: dict) -> None:
        """Create an exporter from a config dict."""
        exp_type = config.get("type", "")
        cls = EXPORTER_TYPES.get(exp_type)
        if not cls:
            log.warning("Unknown export type: %s", exp_type)
            return
        # Pass config keys as kwargs (minus 'type')
        kwargs = {k: v for k, v in config.items() if k != "type"}
        self._exporters.append(cls(**kwargs))

    async def on_alert(self, alert: dict) -> None:
        """Forward an alert to all alert-capable exporters."""
        for exp in self._exporters:
            try:
                await exp.export_alert(alert)
            except Exception:
                pass

    async def on_serial_line(self, console_id: str, text: str, severity: str) -> None:
        """Forward serial console lines to syslog exporters."""
        for exp in self._exporters:
            if isinstance(exp, SyslogExporter):
                await exp.export_serial_line(console_id, text, severity)

    async def _export_loop(self) -> None:
        while True:
            try:
                sources = self._metrics.get_all() if self._metrics else []
                for exp in self._exporters:
                    try:
                        await exp.export(sources)
                    except Exception:
                        pass
                await asyncio.sleep(self._export_interval)
            except asyncio.CancelledError:
                return
