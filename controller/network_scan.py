# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Network Scanning — device discovery and vulnerability assessment.

Scanner stack (layered, each optional):
  nmap      — always available; discovery, port scan, OS/service fingerprint
  nuclei    — OSS CVE/template scanner; fast, covers web + known CVEs
  openvas   — Greenbone/OpenVAS GVM; full professional vuln scanner (OSS)
  nessus    — Tenable Nessus Pro / Essentials; paid, REST API

Findings from all scanners are normalised into ScanFinding records with
a unified severity scale (critical/high/medium/low/info) and fed into
AlertManager + ITSMManager.

Rogue device detection: after every nmap sweep, IPs/MACs not present in
the local node registry or ITAM inventory are flagged and can auto-create
ITSM tickets.

Data lives in:
  controller/scan_data/
    network_scan.json     — config + last scan state
    scan_results.json     — ring buffer of last N scan results
    hosts.json            — current host inventory (updated each sweep)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.network_scan")

_SEVERITIES = ("critical", "high", "medium", "low", "info")
_CVSS_TO_SEVERITY = {
    9.0: "critical",
    7.0: "high",
    4.0: "medium",
    0.1: "low",
    0.0: "info",
}

MAX_RESULTS = 50  # scan history ring buffer


# ── Data models ───────────────────────────────────────────────────────────────

@dataclass
class OpenPort:
    port: int
    protocol: str = "tcp"          # tcp | udp
    state: str = "open"
    service: str = ""
    version: str = ""
    tunnel: str = ""               # ssl | tls etc.

    def to_dict(self) -> dict[str, Any]:
        return {
            "port": self.port, "protocol": self.protocol,
            "state": self.state, "service": self.service,
            "version": self.version, "tunnel": self.tunnel,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "OpenPort":
        return cls(**{k: v for k, v in d.items() if k in cls.__dataclass_fields__})


@dataclass
class DiscoveredHost:
    """Normalized host record from nmap sweep."""
    ip: str
    mac: str = ""
    mac_vendor: str = ""
    hostnames: list[str] = field(default_factory=list)
    os_name: str = ""
    os_accuracy: int = 0
    ports: list[OpenPort] = field(default_factory=list)
    last_seen: float = 0.0
    first_seen: float = 0.0
    # Cross-reference
    in_itam: bool = False          # matched a known node or MDM device
    node_id: str = ""              # ozma node ID if matched

    def to_dict(self) -> dict[str, Any]:
        return {
            "ip": self.ip, "mac": self.mac, "mac_vendor": self.mac_vendor,
            "hostnames": self.hostnames, "os_name": self.os_name,
            "os_accuracy": self.os_accuracy,
            "ports": [p.to_dict() for p in self.ports],
            "last_seen": self.last_seen, "first_seen": self.first_seen,
            "in_itam": self.in_itam, "node_id": self.node_id,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DiscoveredHost":
        ports = [OpenPort.from_dict(p) for p in d.get("ports", [])]
        return cls(
            ip=d["ip"], mac=d.get("mac", ""), mac_vendor=d.get("mac_vendor", ""),
            hostnames=d.get("hostnames", []), os_name=d.get("os_name", ""),
            os_accuracy=d.get("os_accuracy", 0), ports=ports,
            last_seen=d.get("last_seen", 0.0), first_seen=d.get("first_seen", 0.0),
            in_itam=d.get("in_itam", False), node_id=d.get("node_id", ""),
        )


@dataclass
class ScanFinding:
    """Vulnerability / finding from any scanner."""
    id: str                        # scanner-internal ID
    scanner: str                   # "nmap" | "nuclei" | "openvas" | "nessus"
    host: str
    title: str
    severity: str                  # critical/high/medium/low/info
    cvss_score: float = 0.0
    cve_ids: list[str] = field(default_factory=list)
    description: str = ""
    solution: str = ""
    port: int | None = None
    service: str = ""
    plugin_id: str = ""
    suppressed: bool = False
    first_seen: float = 0.0
    last_seen: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "scanner": self.scanner, "host": self.host,
            "title": self.title, "severity": self.severity,
            "cvss_score": self.cvss_score, "cve_ids": self.cve_ids,
            "description": self.description, "solution": self.solution,
            "port": self.port, "service": self.service,
            "plugin_id": self.plugin_id, "suppressed": self.suppressed,
            "first_seen": self.first_seen, "last_seen": self.last_seen,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "ScanFinding":
        return cls(
            id=d["id"], scanner=d["scanner"], host=d["host"],
            title=d["title"], severity=d.get("severity", "info"),
            cvss_score=d.get("cvss_score", 0.0),
            cve_ids=d.get("cve_ids", []),
            description=d.get("description", ""),
            solution=d.get("solution", ""),
            port=d.get("port"), service=d.get("service", ""),
            plugin_id=d.get("plugin_id", ""),
            suppressed=d.get("suppressed", False),
            first_seen=d.get("first_seen", 0.0),
            last_seen=d.get("last_seen", 0.0),
        )


@dataclass
class ScanResult:
    """One complete scan run."""
    id: str
    started_at: float
    finished_at: float = 0.0
    targets: list[str] = field(default_factory=list)
    hosts_found: int = 0
    new_hosts: int = 0
    rogue_hosts: int = 0
    findings_by_severity: dict[str, int] = field(default_factory=dict)
    scanners_used: list[str] = field(default_factory=list)
    error: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "started_at": self.started_at,
            "finished_at": self.finished_at, "targets": self.targets,
            "hosts_found": self.hosts_found, "new_hosts": self.new_hosts,
            "rogue_hosts": self.rogue_hosts,
            "findings_by_severity": self.findings_by_severity,
            "scanners_used": self.scanners_used, "error": self.error,
        }


# ── Config ────────────────────────────────────────────────────────────────────

@dataclass
class NessusConfig:
    url: str = ""                  # https://nessus.company.com:8834
    access_key_env: str = ""       # env var: NESSUS_ACCESS_KEY
    secret_key_env: str = ""       # env var: NESSUS_SECRET_KEY
    policy_id: str = ""            # scan policy UUID (blank = default basic scan)
    folder_id: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "url": self.url,
            "access_key_env": self.access_key_env,
            "secret_key_env": self.secret_key_env,
            "policy_id": self.policy_id,
            "folder_id": self.folder_id,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "NessusConfig":
        return cls(
            url=d.get("url", ""), access_key_env=d.get("access_key_env", ""),
            secret_key_env=d.get("secret_key_env", ""),
            policy_id=d.get("policy_id", ""), folder_id=d.get("folder_id", 0),
        )


# ── Trivy scanner (container image CVE scanner) ───────────────────────────────

@dataclass
class TrivyConfig:
    """Configuration for Trivy container image vulnerability scanner."""
    enabled: bool = False          # Enable/disable Trivy scanning
    images: list[str] = field(default_factory=list)  # Docker images to scan
    severity: str = "critical,high,medium"  # comma-separated severity levels

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "images": self.images,
            "severity": self.severity,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "TrivyConfig":
        return cls(
            enabled=d.get("enabled", False),
            images=d.get("images", []),
            severity=d.get("severity", "critical,high,medium"),
        )


# ── Trufflehog scanner (git secrets scanner) ───────────────────────────────────

@dataclass
class TrufflehogConfig:
    """Configuration for Trufflehog git secrets scanner."""
    enabled: bool = False          # Enable/disable Trufflehog scanning
    paths: list[str] = field(default_factory=list)  # Git repos/paths to scan
    severity: str = "critical,high,medium"  # severity filter

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "paths": self.paths,
            "severity": self.severity,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "TrufflehogConfig":
        return cls(
            enabled=d.get("enabled", False),
            paths=d.get("paths", []),
            severity=d.get("severity", "critical,high,medium"),
        )


@dataclass
class OpenVASConfig:
    host: str = "localhost"
    port: int = 9390               # GVM XML protocol port (gvmd)
    username: str = "admin"
    password_env: str = ""         # env var: OPENVAS_PASSWORD
    scan_config: str = "daba56c8-73ec-11df-a475-002264764cea"  # Full and Fast

    def to_dict(self) -> dict[str, Any]:
        return {
            "host": self.host, "port": self.port,
            "username": self.username, "password_env": self.password_env,
            "scan_config": self.scan_config,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "OpenVASConfig":
        return cls(
            host=d.get("host", "localhost"), port=d.get("port", 9390),
            username=d.get("username", "admin"),
            password_env=d.get("password_env", ""),
            scan_config=d.get("scan_config", "daba56c8-73ec-11df-a475-002264764cea"),
        )


@dataclass
class NetworkScanConfig:
    # Targets — CIDR ranges to scan (default: detect from local interfaces)
    targets: list[str] = field(default_factory=list)

    # Schedule (seconds between scans; 0 = manual only)
    discovery_interval: int = 3600        # light nmap sweep every hour
    vuln_scan_interval: int = 86400       # full vuln scan once a day

    # Rogue device handling
    rogue_device_alert: bool = True
    rogue_device_itsm_ticket: bool = True
    rogue_device_itsm_priority: str = "high"

    # Nmap options
    nmap_args: str = "-sV -O --script=default,vuln"  # extra nmap args

    # Nuclei (OSS CVE scanner)
    nuclei_enabled: bool = True
    nuclei_severity: str = "critical,high,medium"    # comma-separated
    nuclei_templates: str = ""           # blank = nuclei default templates

    # OpenVAS / Greenbone (OSS full vuln scanner)
    openvas_enabled: bool = False
    openvas: OpenVASConfig = field(default_factory=OpenVASConfig)

    # Nessus Pro (paid)
    nessus_enabled: bool = False
    nessus: NessusConfig = field(default_factory=NessusConfig)

    # Auto-create ITSM tickets for findings at or above this severity
    ticket_severity_threshold: str = "high"

    def to_dict(self) -> dict[str, Any]:
        return {
            "targets": self.targets,
            "discovery_interval": self.discovery_interval,
            "vuln_scan_interval": self.vuln_scan_interval,
            "rogue_device_alert": self.rogue_device_alert,
            "rogue_device_itsm_ticket": self.rogue_device_itsm_ticket,
            "rogue_device_itsm_priority": self.rogue_device_itsm_priority,
            "nmap_args": self.nmap_args,
            "nuclei_enabled": self.nuclei_enabled,
            "nuclei_severity": self.nuclei_severity,
            "nuclei_templates": self.nuclei_templates,
            "openvas_enabled": self.openvas_enabled,
            "openvas": self.openvas.to_dict(),
            "nessus_enabled": self.nessus_enabled,
            "nessus": self.nessus.to_dict(),
            "ticket_severity_threshold": self.ticket_severity_threshold,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "NetworkScanConfig":
        return cls(
            targets=d.get("targets", []),
            discovery_interval=d.get("discovery_interval", 3600),
            vuln_scan_interval=d.get("vuln_scan_interval", 86400),
            rogue_device_alert=d.get("rogue_device_alert", True),
            rogue_device_itsm_ticket=d.get("rogue_device_itsm_ticket", True),
            rogue_device_itsm_priority=d.get("rogue_device_itsm_priority", "high"),
            nmap_args=d.get("nmap_args", "-sV -O --script=default,vuln"),
            nuclei_enabled=d.get("nuclei_enabled", True),
            nuclei_severity=d.get("nuclei_severity", "critical,high,medium"),
            nuclei_templates=d.get("nuclei_templates", ""),
            openvas_enabled=d.get("openvas_enabled", False),
            openvas=OpenVASConfig.from_dict(d.get("openvas", {})),
            nessus_enabled=d.get("nessus_enabled", False),
            nessus=NessusConfig.from_dict(d.get("nessus", {})),
            ticket_severity_threshold=d.get("ticket_severity_threshold", "high"),
        )


# ── Nmap scanner ──────────────────────────────────────────────────────────────

def _cvss_to_severity(score: float) -> str:
    for threshold, sev in sorted(_CVSS_TO_SEVERITY.items(), reverse=True):
        if score >= threshold:
            return sev
    return "info"


def _parse_nmap_xml(xml_text: str) -> tuple[list[DiscoveredHost], list[ScanFinding]]:
    """Parse nmap -oX output into hosts and NSE script findings."""
    hosts: list[DiscoveredHost] = []
    findings: list[ScanFinding] = []
    now = time.time()
    try:
        root = ET.fromstring(xml_text)
    except ET.ParseError as e:
        log.warning("nmap XML parse error: %s", e)
        return hosts, findings

    for host_el in root.findall("host"):
        if host_el.find("status") is None:
            continue
        status = host_el.find("status")
        if status is not None and status.get("state") != "up":
            continue

        ip = ""
        mac = ""
        mac_vendor = ""
        for addr in host_el.findall("address"):
            if addr.get("addrtype") == "ipv4":
                ip = addr.get("addr", "")
            elif addr.get("addrtype") == "mac":
                mac = addr.get("addr", "")
                mac_vendor = addr.get("vendor", "")
        if not ip:
            continue

        hostnames = []
        for hn in host_el.findall("hostnames/hostname"):
            name = hn.get("name", "")
            if name:
                hostnames.append(name)

        os_name = ""
        os_accuracy = 0
        os_el = host_el.find("os")
        if os_el is not None:
            best = None
            for osmatch in os_el.findall("osmatch"):
                acc = int(osmatch.get("accuracy", "0"))
                if acc > os_accuracy:
                    os_name = osmatch.get("name", "")
                    os_accuracy = acc
                    best = osmatch

        ports: list[OpenPort] = []
        for port_el in host_el.findall("ports/port"):
            state_el = port_el.find("state")
            if state_el is None or state_el.get("state") not in ("open", "open|filtered"):
                continue
            port_num = int(port_el.get("portid", "0"))
            proto = port_el.get("protocol", "tcp")
            service_el = port_el.find("service")
            svc_name = ""
            svc_version = ""
            tunnel = ""
            if service_el is not None:
                svc_name = service_el.get("name", "")
                product = service_el.get("product", "")
                version = service_el.get("version", "")
                svc_version = f"{product} {version}".strip()
                tunnel = service_el.get("tunnel", "")
            ports.append(OpenPort(
                port=port_num, protocol=proto,
                state="open", service=svc_name,
                version=svc_version, tunnel=tunnel,
            ))

            # NSE script findings on this port
            for script_el in port_el.findall("script"):
                script_id = script_el.get("id", "")
                output = script_el.get("output", "")
                if not output or "ERROR" in output.upper() and len(output) < 20:
                    continue
                # Only surface scripts that indicate actual issues
                severity = "info"
                cves: list[str] = []
                cvss = 0.0
                for elem in script_el.findall(".//elem[@key]"):
                    k = elem.get("key", "")
                    v = elem.text or ""
                    if k == "cvss":
                        try:
                            cvss = float(v)
                            severity = _cvss_to_severity(cvss)
                        except ValueError:
                            pass
                    elif k.startswith("CVE"):
                        cves.append(v if v.startswith("CVE") else k)
                cve_pat = re.findall(r"CVE-\d{4}-\d+", output)
                cves.extend(cve_pat)
                cves = list(dict.fromkeys(cves))
                if script_id in ("vuln", "vulners") or cves or cvss >= 4.0:
                    severity = _cvss_to_severity(cvss) if cvss > 0 else ("medium" if cves else "info")
                    findings.append(ScanFinding(
                        id=f"nmap-{ip}-{port_num}-{script_id}",
                        scanner="nmap",
                        host=ip,
                        title=f"{script_id} on {svc_name or str(port_num)}/{proto}",
                        severity=severity,
                        cvss_score=cvss,
                        cve_ids=cves,
                        description=output[:2000],
                        port=port_num,
                        service=svc_name,
                        plugin_id=script_id,
                        first_seen=now,
                        last_seen=now,
                    ))

        hosts.append(DiscoveredHost(
            ip=ip, mac=mac, mac_vendor=mac_vendor,
            hostnames=hostnames, os_name=os_name, os_accuracy=os_accuracy,
            ports=ports, last_seen=now, first_seen=now,
        ))

    return hosts, findings


class NmapScanner:
    """Runs nmap as a subprocess and parses XML output."""

    def __init__(self, extra_args: str = "-sV -O --script=default,vuln") -> None:
        self._extra_args = extra_args

    async def available(self) -> bool:
        try:
            p = await asyncio.create_subprocess_exec(
                "nmap", "--version",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await p.communicate()
            return p.returncode == 0
        except FileNotFoundError:
            return False

    async def scan(self, targets: list[str]) -> tuple[list[DiscoveredHost], list[ScanFinding]]:
        if not targets:
            return [], []
        target_str = " ".join(targets)
        args = ["nmap", "-oX", "-"] + self._extra_args.split() + targets
        try:
            p = await asyncio.create_subprocess_exec(
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(p.communicate(), timeout=600)
        except asyncio.TimeoutError:
            log.warning("nmap scan timed out for %s", target_str)
            return [], []
        except FileNotFoundError:
            log.error("nmap not found — install with: apt install nmap")
            return [], []
        if p.returncode != 0:
            log.warning("nmap exited %d: %s", p.returncode, stderr.decode()[:200])
        return _parse_nmap_xml(stdout.decode(errors="replace"))

    async def discovery_scan(self, targets: list[str]) -> list[DiscoveredHost]:
        """Fast discovery sweep: ping + port 22/80/443/445 only, no scripts."""
        if not targets:
            return []
        args = ["nmap", "-oX", "-", "-sn", "--min-hostgroup=64"] + targets
        try:
            p = await asyncio.create_subprocess_exec(
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(p.communicate(), timeout=120)
        except (asyncio.TimeoutError, FileNotFoundError):
            return []
        hosts, _ = _parse_nmap_xml(stdout.decode(errors="replace"))
        return hosts


# ── Nuclei scanner ────────────────────────────────────────────────────────────

def _parse_nuclei_jsonl(output: str) -> list[ScanFinding]:
    findings: list[ScanFinding] = []
    now = time.time()
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        info = item.get("info", {})
        host = item.get("host", "")
        matched_at = item.get("matched-at", host)
        template_id = item.get("template-id", "")
        severity = info.get("severity", "info").lower()
        if severity not in _SEVERITIES:
            severity = "info"
        cves: list[str] = []
        classification = info.get("classification", {})
        cves = classification.get("cve-id", [])
        if isinstance(cves, str):
            cves = [cves]
        cvss = 0.0
        for field_name in ("cvss-metrics", "cvss-score"):
            v = classification.get(field_name, "")
            if isinstance(v, (int, float)):
                cvss = float(v)
            elif isinstance(v, str):
                try:
                    cvss = float(v.split("/")[0])
                except (ValueError, IndexError):
                    pass
        # Extract port from matched_at (e.g. http://10.0.0.1:8080/path)
        port = None
        m = re.search(r":(\d+)", matched_at.split("//")[-1] if "//" in matched_at else matched_at)
        if m:
            try:
                port = int(m.group(1))
            except ValueError:
                pass
        ip = host.split("://")[-1].split(":")[0].split("/")[0]
        findings.append(ScanFinding(
            id=f"nuclei-{ip}-{template_id}-{port or 0}",
            scanner="nuclei",
            host=ip,
            title=info.get("name", template_id),
            severity=severity,
            cvss_score=cvss,
            cve_ids=cves,
            description=info.get("description", ""),
            solution=info.get("remediation", ""),
            port=port,
            service="",
            plugin_id=template_id,
            first_seen=now,
            last_seen=now,
        ))
    return findings


class NucleiScanner:
    """Runs nuclei as a subprocess, parses JSONL output."""

    def __init__(self, severity: str = "critical,high,medium",
                 templates: str = "") -> None:
        self._severity = severity
        self._templates = templates

    async def available(self) -> bool:
        try:
            p = await asyncio.create_subprocess_exec(
                "nuclei", "-version",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await p.communicate()
            return p.returncode == 0
        except FileNotFoundError:
            return False

    async def scan(self, targets: list[str]) -> list[ScanFinding]:
        if not targets:
            return []
        args = ["nuclei", "-json", "-silent",
                "-severity", self._severity,
                "-u", ",".join(targets)]
        if self._templates:
            args += ["-t", self._templates]
        try:
            p = await asyncio.create_subprocess_exec(
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(p.communicate(), timeout=900)
        except asyncio.TimeoutError:
            log.warning("nuclei scan timed out")
            return []
        except FileNotFoundError:
            log.info("nuclei not installed — skipping (install: go install github.com/projectdiscovery/nuclei/v3/cmd/nuclei@latest)")
            return []
        return _parse_nuclei_jsonl(stdout.decode(errors="replace"))


# ── OpenVAS / Greenbone scanner ───────────────────────────────────────────────

class OpenVASScanner:
    """
    Greenbone Vulnerability Manager (GVM) via the XML protocol on port 9390.

    GVM speaks a simple XML request/response protocol over TCP (or Unix socket).
    We open a raw TCP connection, authenticate, create a target + task, launch
    it, poll until done, then retrieve results.

    Deploy OpenVAS locally:
      docker run -d -p 9390:9390 greenbone/community-edition
    """

    _TIMEOUT = 1800  # 30 min max for a vuln scan

    def __init__(self, config: OpenVASConfig) -> None:
        self._cfg = config

    async def available(self) -> bool:
        try:
            r, w = await asyncio.wait_for(
                asyncio.open_connection(self._cfg.host, self._cfg.port),
                timeout=5,
            )
            w.close()
            await w.wait_closed()
            return True
        except Exception:
            return False

    async def _send(self, writer: asyncio.StreamWriter,
                    reader: asyncio.StreamReader, xml: str) -> str:
        writer.write(xml.encode())
        await writer.drain()
        buf = b""
        while True:
            chunk = await asyncio.wait_for(reader.read(65536), timeout=30)
            if not chunk:
                break
            buf += chunk
            # GVM sends complete XML elements; check if we have a closing tag
            # corresponding to the root element
            try:
                ET.fromstring(buf.decode(errors="replace"))
                break
            except ET.ParseError:
                continue
        return buf.decode(errors="replace")

    async def scan(self, targets: list[str]) -> list[ScanFinding]:
        password = os.environ.get(self._cfg.password_env, "")
        if not password:
            log.warning("OpenVAS password env var %s not set", self._cfg.password_env)
            return []

        findings: list[ScanFinding] = []
        now = time.time()
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(self._cfg.host, self._cfg.port),
                timeout=10,
            )
        except Exception as e:
            log.error("Cannot connect to OpenVAS at %s:%d — %s",
                      self._cfg.host, self._cfg.port, e)
            return []

        try:
            # Authenticate
            auth_xml = (
                f'<authenticate><credentials>'
                f'<username>{self._cfg.username}</username>'
                f'<password>{password}</password>'
                f'</credentials></authenticate>'
            )
            resp = await self._send(writer, reader, auth_xml)
            if 'status="200"' not in resp:
                log.error("OpenVAS auth failed: %s", resp[:200])
                return []

            # Create a target
            target_str = ",".join(targets)
            create_target = (
                f'<create_target>'
                f'<name>ozma-scan-{int(now)}</name>'
                f'<hosts>{target_str}</hosts>'
                f'<alive_tests>ICMP, TCP-ACK Service, ARP</alive_tests>'
                f'</create_target>'
            )
            resp = await self._send(writer, reader, create_target)
            target_id = re.search(r'id="([^"]+)"', resp)
            if not target_id:
                log.error("OpenVAS create_target failed: %s", resp[:200])
                return []
            target_id_str = target_id.group(1)

            # Create a task
            create_task = (
                f'<create_task>'
                f'<name>ozma-task-{int(now)}</name>'
                f'<config id="{self._cfg.scan_config}"/>'
                f'<target id="{target_id_str}"/>'
                f'</create_task>'
            )
            resp = await self._send(writer, reader, create_task)
            task_id = re.search(r'id="([^"]+)"', resp)
            if not task_id:
                log.error("OpenVAS create_task failed: %s", resp[:200])
                return []
            task_id_str = task_id.group(1)

            # Start the task
            await self._send(writer, reader, f'<start_task task_id="{task_id_str}"/>')

            # Poll until done
            start = time.time()
            while time.time() - start < self._TIMEOUT:
                await asyncio.sleep(30)
                resp = await self._send(writer, reader,
                                        f'<get_tasks task_id="{task_id_str}"/>')
                status_m = re.search(r'<status>([^<]+)</status>', resp)
                if status_m and status_m.group(1) in ("Done", "Stopped"):
                    break
                log.debug("OpenVAS task %s status: %s", task_id_str,
                          status_m.group(1) if status_m else "unknown")
            else:
                log.warning("OpenVAS scan timed out")
                return []

            # Get results
            resp = await self._send(writer, reader,
                                    f'<get_results task_id="{task_id_str}"/>')
            findings = _parse_openvas_xml(resp, now)

            # Cleanup
            await self._send(writer, reader, f'<delete_task task_id="{task_id_str}" ultimate="true"/>')
            await self._send(writer, reader, f'<delete_target target_id="{target_id_str}" ultimate="true"/>')

        except Exception as e:
            log.error("OpenVAS scan error: %s", e)
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

        return findings


def _parse_openvas_xml(xml_text: str, now: float) -> list[ScanFinding]:
    findings: list[ScanFinding] = []
    try:
        root = ET.fromstring(xml_text)
    except ET.ParseError:
        return findings
    for result in root.findall(".//result"):
        rid = result.get("id", "")
        host_el = result.find("host")
        host = host_el.text.strip() if host_el is not None and host_el.text else ""
        name_el = result.find("name")
        title = name_el.text.strip() if name_el is not None and name_el.text else "Finding"
        desc_el = result.find("description")
        description = desc_el.text.strip() if desc_el is not None and desc_el.text else ""
        solution_el = result.find("solution")
        solution = solution_el.text.strip() if solution_el is not None and solution_el.text else ""
        threat_el = result.find("threat")
        threat = (threat_el.text or "").lower() if threat_el is not None else ""
        severity_map = {
            "high": "high", "medium": "medium", "low": "low",
            "log": "info", "false positive": "info", "error": "info",
        }
        severity = severity_map.get(threat, "info")
        cvss = 0.0
        cvss_el = result.find(".//cvss_base")
        if cvss_el is not None and cvss_el.text:
            try:
                cvss = float(cvss_el.text)
                severity = _cvss_to_severity(cvss)
            except ValueError:
                pass
        cves: list[str] = []
        for ref in result.findall(".//ref[@type='cve']"):
            if ref.get("id"):
                cves.append(ref.get("id"))
        port = None
        port_el = result.find("port")
        if port_el is not None and port_el.text:
            m = re.match(r"(\d+)", port_el.text)
            if m:
                try:
                    port = int(m.group(1))
                except ValueError:
                    pass
        nvt_el = result.find("nvt")
        plugin_id = nvt_el.get("oid", "") if nvt_el is not None else ""
        findings.append(ScanFinding(
            id=f"openvas-{rid or host}-{plugin_id}",
            scanner="openvas",
            host=host,
            title=title,
            severity=severity,
            cvss_score=cvss,
            cve_ids=cves,
            description=description[:2000],
            solution=solution[:1000],
            port=port,
            plugin_id=plugin_id,
            first_seen=now,
            last_seen=now,
        ))
    return findings


# ── Nessus scanner (paid) ─────────────────────────────────────────────────────

class NessusScanner:
    """
    Tenable Nessus Pro / Essentials via the REST API.

    Authentication: API key pair (access_key + secret_key).
    For Essentials, limited to 16 IPs.
    For Pro / Tenable.io, unlimited.

    Credentials are stored as env var names (never stored directly).
    """

    _TIMEOUT = 3600

    def __init__(self, config: NessusConfig) -> None:
        self._cfg = config
        self._headers: dict[str, str] = {}

    def _auth_headers(self) -> dict[str, str]:
        access = os.environ.get(self._cfg.access_key_env, "")
        secret = os.environ.get(self._cfg.secret_key_env, "")
        return {
            "X-ApiKeys": f"accessKey={access}; secretKey={secret}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        }

    async def available(self) -> bool:
        if not self._cfg.url:
            return False
        access = os.environ.get(self._cfg.access_key_env, "")
        return bool(access)

    async def _request(self, method: str, path: str,
                       body: dict | None = None) -> dict:
        import urllib.request as req
        url = self._cfg.url.rstrip("/") + path
        data = json.dumps(body).encode() if body else None
        headers = self._auth_headers()
        http_req = req.Request(url, data=data, headers=headers, method=method)
        def _do():
            import ssl
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            with req.urlopen(http_req, context=ctx, timeout=30) as resp:
                return json.loads(resp.read())
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, _do)

    async def scan(self, targets: list[str]) -> list[ScanFinding]:
        now = time.time()
        findings: list[ScanFinding] = []
        try:
            # Create scan
            scan_body: dict[str, Any] = {
                "uuid": self._cfg.policy_id or "bbd4f805-3966-d464-b2d1-0079eb89d69710cde5f3efed",
                "settings": {
                    "name": f"ozma-scan-{int(now)}",
                    "text_targets": "\n".join(targets),
                    "folder_id": self._cfg.folder_id or 0,
                    "enabled": False,
                    "launch": "ON_DEMAND",
                },
            }
            result = await self._request("POST", "/scans", scan_body)
            scan_id = result.get("scan", {}).get("id")
            if not scan_id:
                log.error("Nessus create scan failed: %s", result)
                return []

            # Launch
            await self._request("POST", f"/scans/{scan_id}/launch")

            # Poll
            start = time.time()
            while time.time() - start < self._TIMEOUT:
                await asyncio.sleep(15)
                detail = await self._request("GET", f"/scans/{scan_id}")
                status = detail.get("info", {}).get("status", "")
                if status in ("completed", "canceled", "aborted"):
                    break
                log.debug("Nessus scan %s: %s", scan_id, status)
            else:
                log.warning("Nessus scan timed out")
                return []

            # Results
            detail = await self._request("GET", f"/scans/{scan_id}")
            for vuln in detail.get("vulnerabilities", []):
                host_id = vuln.get("host_id")
                severity_id = vuln.get("severity", 0)
                sev_map = {0: "info", 1: "low", 2: "medium", 3: "high", 4: "critical"}
                severity = sev_map.get(severity_id, "info")
                plugin_id = str(vuln.get("plugin_id", ""))
                plugin_name = vuln.get("plugin_name", "")
                count = vuln.get("count", 1)
                # Host details
                host_detail = await self._request("GET",
                                                   f"/scans/{scan_id}/hosts/{host_id}")
                host_ip = host_detail.get("info", {}).get("host-ip", str(host_id))
                for h_vuln in host_detail.get("vulnerabilities", []):
                    if str(h_vuln.get("plugin_id")) != plugin_id:
                        continue
                    output_data = await self._request(
                        "GET",
                        f"/scans/{scan_id}/hosts/{host_id}/plugins/{plugin_id}",
                    )
                    desc = ""
                    solution = ""
                    cves: list[str] = []
                    cvss = 0.0
                    for attr in output_data.get("info", {}).get("plugindescription",
                                                                  {}).get("pluginattributes",
                                                                           {}).get("ref_information",
                                                                                    {}).get("ref", []):
                        if attr.get("name") == "cve":
                            for val in attr.get("values", {}).get("value", []):
                                if val:
                                    cves.append(val)
                    plugin_attrs = (output_data.get("info", {})
                                    .get("plugindescription", {})
                                    .get("pluginattributes", {}))
                    desc = plugin_attrs.get("description", "")
                    solution = plugin_attrs.get("solution", "")
                    cvss_str = plugin_attrs.get("risk_information", {}).get("cvss3_base_score", "")
                    if not cvss_str:
                        cvss_str = plugin_attrs.get("risk_information", {}).get("cvss_base_score", "")
                    try:
                        cvss = float(cvss_str)
                    except (ValueError, TypeError):
                        pass
                    port = h_vuln.get("port")
                    findings.append(ScanFinding(
                        id=f"nessus-{host_ip}-{plugin_id}",
                        scanner="nessus",
                        host=host_ip,
                        title=plugin_name or h_vuln.get("plugin_name", ""),
                        severity=severity,
                        cvss_score=cvss,
                        cve_ids=cves,
                        description=str(desc)[:2000],
                        solution=str(solution)[:1000],
                        port=port,
                        plugin_id=plugin_id,
                        first_seen=now,
                        last_seen=now,
                    ))

            # Cleanup
            await self._request("DELETE", f"/scans/{scan_id}")
        except Exception as e:
            log.error("Nessus scan error: %s", e)

        return findings


# ── Manager ───────────────────────────────────────────────────────────────────

class NetworkScanManager:
    """
    Orchestrates all scanners, manages host inventory, fires alerts/tickets.

    itsm and alert_mgr are optional callbacks — set after construction to
    avoid circular imports:
        mgr = NetworkScanManager(path, config)
        mgr.itsm = itsm_mgr
        mgr.alert_mgr = alert_mgr
    """

    def __init__(self, data_path: Path,
                 config: NetworkScanConfig | None = None,
                 event_queue: asyncio.Queue | None = None) -> None:
        self._path = data_path
        self._config = config or NetworkScanConfig()
        self._event_queue = event_queue
        self._hosts: dict[str, DiscoveredHost] = {}    # ip → host
        self._findings: dict[str, ScanFinding] = {}    # id → finding
        self._results: list[ScanResult] = []
        self._tasks: list[asyncio.Task] = []
        self._last_discovery: float = 0.0
        self._last_vuln_scan: float = 0.0
        self.itsm: Any = None
        self.alert_mgr: Any = None
        self._load()

    # ── Persistence ───────────────────────────────────────────────────

    def _load(self) -> None:
        self._path.mkdir(parents=True, exist_ok=True)
        cfg_path = self._path / "network_scan.json"
        if cfg_path.exists():
            try:
                d = json.loads(cfg_path.read_text())
                self._config = NetworkScanConfig.from_dict(d.get("config", {}))
                self._last_discovery = d.get("last_discovery", 0.0)
                self._last_vuln_scan = d.get("last_vuln_scan", 0.0)
            except Exception as e:
                log.warning("Failed to load network scan config: %s", e)
        hosts_path = self._path / "hosts.json"
        if hosts_path.exists():
            try:
                raw = json.loads(hosts_path.read_text())
                self._hosts = {h["ip"]: DiscoveredHost.from_dict(h)
                               for h in raw.get("hosts", [])}
                self._findings = {f["id"]: ScanFinding.from_dict(f)
                                  for f in raw.get("findings", [])}
            except Exception as e:
                log.warning("Failed to load scan hosts: %s", e)
        results_path = self._path / "scan_results.json"
        if results_path.exists():
            try:
                raw = json.loads(results_path.read_text())
                # ScanResult is lightweight — just reconstruct from dict
                self._results = [ScanResult(**{
                    k: v for k, v in r.items()
                    if k in ScanResult.__dataclass_fields__
                }) for r in raw.get("results", [])]
            except Exception as e:
                log.warning("Failed to load scan results: %s", e)
        log.info("Network scan loaded: %d hosts, %d findings, %d results",
                 len(self._hosts), len(self._findings), len(self._results))

    def _save(self) -> None:
        cfg_path = self._path / "network_scan.json"
        try:
            cfg_path.write_text(json.dumps({
                "config": self._config.to_dict(),
                "last_discovery": self._last_discovery,
                "last_vuln_scan": self._last_vuln_scan,
            }, indent=2))
        except Exception as e:
            log.error("Failed to save network scan config: %s", e)
        hosts_path = self._path / "hosts.json"
        try:
            hosts_path.write_text(json.dumps({
                "hosts": [h.to_dict() for h in self._hosts.values()],
                "findings": [f.to_dict() for f in self._findings.values()],
            }, indent=2))
        except Exception as e:
            log.error("Failed to save hosts: %s", e)
        results_path = self._path / "scan_results.json"
        try:
            results_path.write_text(json.dumps({
                "results": [r.to_dict() for r in self._results[-MAX_RESULTS:]],
            }, indent=2))
        except Exception as e:
            log.error("Failed to save scan results: %s", e)

    # ── Lifecycle ─────────────────────────────────────────────────────

    async def start(self) -> None:
        self._tasks.append(asyncio.create_task(
            self._scan_loop(), name="network-scan-loop"
        ))
        log.info("Network scan manager started")

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()

    async def _scan_loop(self) -> None:
        await asyncio.sleep(30)  # brief startup delay
        while True:
            now = time.time()
            try:
                if (self._config.discovery_interval > 0 and
                        now - self._last_discovery >= self._config.discovery_interval):
                    await self.run_discovery()
                if (self._config.vuln_scan_interval > 0 and
                        now - self._last_vuln_scan >= self._config.vuln_scan_interval):
                    await self.run_vuln_scan()
            except Exception as e:
                log.error("Scan loop error: %s", e)
            await asyncio.sleep(60)

    # ── Target resolution ─────────────────────────────────────────────

    def _effective_targets(self) -> list[str]:
        if self._config.targets:
            return self._config.targets
        # Auto-detect local subnets
        return self._detect_local_subnets()

    def _detect_local_subnets(self) -> list[str]:
        """Best-effort local subnet detection without external libraries."""
        subnets: list[str] = []
        try:
            import subprocess
            result = subprocess.run(
                ["ip", "-4", "route"],
                capture_output=True, text=True, timeout=5,
            )
            for line in result.stdout.splitlines():
                parts = line.split()
                if parts and "/" in parts[0] and not parts[0].startswith("169.254"):
                    subnets.append(parts[0])
        except Exception:
            pass
        return subnets or ["192.168.1.0/24"]

    # ── Discovery scan ────────────────────────────────────────────────

    async def run_discovery(self) -> dict[str, Any]:
        """Fast nmap ping sweep — find live hosts, no vuln scanning."""
        targets = self._effective_targets()
        if not targets:
            return {"ok": False, "error": "No targets configured"}

        now = time.time()
        scan_id = f"disc-{int(now)}"
        result = ScanResult(
            id=scan_id, started_at=now, targets=targets,
            scanners_used=["nmap-discovery"],
        )

        nmap = NmapScanner(self._config.nmap_args)
        if not await nmap.available():
            result.error = "nmap not available"
            result.finished_at = time.time()
            self._results.append(result)
            self._save()
            return {"ok": False, "error": result.error}

        fresh_hosts = await nmap.discovery_scan(targets)
        new_hosts = 0
        rogue_hosts = 0

        for host in fresh_hosts:
            existing = self._hosts.get(host.ip)
            if not existing:
                host.first_seen = now
                self._hosts[host.ip] = host
                new_hosts += 1
                if self._config.rogue_device_alert:
                    await self._handle_rogue_device(host)
                    rogue_hosts += 1
            else:
                host.first_seen = existing.first_seen
                host.in_itam = existing.in_itam
                host.node_id = existing.node_id
                self._hosts[host.ip] = host

        self._last_discovery = now
        result.finished_at = time.time()
        result.hosts_found = len(fresh_hosts)
        result.new_hosts = new_hosts
        result.rogue_hosts = rogue_hosts
        self._results.append(result)
        self._save()

        await self._fire_event("network_scan.discovery.complete", {
            "scan_id": scan_id, "hosts_found": len(fresh_hosts),
            "new_hosts": new_hosts, "rogue_hosts": rogue_hosts,
        })
        log.info("Discovery: %d hosts (%d new, %d rogue)", len(fresh_hosts),
                 new_hosts, rogue_hosts)
        return result.to_dict()

    # ── Vulnerability scan ────────────────────────────────────────────

    async def run_vuln_scan(self) -> dict[str, Any]:
        """Full vulnerability scan using enabled scanners."""
        targets = self._effective_targets()
        if not targets:
            return {"ok": False, "error": "No targets configured"}

        now = time.time()
        scan_id = f"vuln-{int(now)}"
        result = ScanResult(id=scan_id, started_at=now, targets=targets)
        all_findings: list[ScanFinding] = []
        scanners_used: list[str] = []

        # 1. Nmap (always — also updates host inventory)
        nmap = NmapScanner(self._config.nmap_args)
        if await nmap.available():
            hosts, nmap_findings = await nmap.scan(targets)
            self._update_hosts(hosts, now)
            all_findings.extend(nmap_findings)
            scanners_used.append("nmap")
            log.info("nmap: %d hosts, %d findings", len(hosts), len(nmap_findings))

        # 2. Nuclei (OSS CVE scanner)
        if self._config.nuclei_enabled:
            nuclei = NucleiScanner(
                severity=self._config.nuclei_severity,
                templates=self._config.nuclei_templates,
            )
            if await nuclei.available():
                # Feed live IPs from host inventory
                live_ips = list(self._hosts.keys())
                nuclei_findings = await nuclei.scan(live_ips)
                all_findings.extend(nuclei_findings)
                scanners_used.append("nuclei")
                log.info("nuclei: %d findings", len(nuclei_findings))

        # 3. OpenVAS (OSS full scanner)
        if self._config.openvas_enabled:
            openvas = OpenVASScanner(self._config.openvas)
            if await openvas.available():
                ov_findings = await openvas.scan(targets)
                all_findings.extend(ov_findings)
                scanners_used.append("openvas")
                log.info("openvas: %d findings", len(ov_findings))

        # 4. Nessus (paid)
        if self._config.nessus_enabled:
            nessus = NessusScanner(self._config.nessus)
            if await nessus.available():
                nessus_findings = await nessus.scan(targets)
                all_findings.extend(nessus_findings)
                scanners_used.append("nessus")
                log.info("nessus: %d findings", len(nessus_findings))

        # Merge findings (preserve suppressed flag, update last_seen)
        for f in all_findings:
            existing = self._findings.get(f.id)
            if existing:
                f.first_seen = existing.first_seen
                f.suppressed = existing.suppressed
            self._findings[f.id] = f

        # Count by severity
        sev_counts: dict[str, int] = {s: 0 for s in _SEVERITIES}
        for f in self._findings.values():
            if not f.suppressed:
                sev_counts[f.severity] = sev_counts.get(f.severity, 0) + 1

        # Alert + ITSM tickets for new high/critical findings
        threshold_idx = _SEVERITIES.index(self._config.ticket_severity_threshold)
        for f in all_findings:
            if f.suppressed:
                continue
            f_idx = _SEVERITIES.index(f.severity) if f.severity in _SEVERITIES else 99
            if f_idx <= threshold_idx:
                await self._handle_finding(f)

        self._last_vuln_scan = now
        result.finished_at = time.time()
        result.hosts_found = len(self._hosts)
        result.findings_by_severity = sev_counts
        result.scanners_used = scanners_used
        self._results.append(result)
        self._save()

        await self._fire_event("network_scan.vuln_scan.complete", {
            "scan_id": scan_id,
            "findings_by_severity": sev_counts,
            "scanners": scanners_used,
        })
        return result.to_dict()

    def _update_hosts(self, fresh: list[DiscoveredHost], now: float) -> None:
        for host in fresh:
            existing = self._hosts.get(host.ip)
            if existing:
                host.first_seen = existing.first_seen
                host.in_itam = existing.in_itam
                host.node_id = existing.node_id
            else:
                host.first_seen = now
            self._hosts[host.ip] = host

    # ── Rogue device handling ─────────────────────────────────────────

    async def _handle_rogue_device(self, host: DiscoveredHost) -> None:
        await self._fire_event("network_scan.rogue_device", {
            "ip": host.ip, "mac": host.mac,
            "mac_vendor": host.mac_vendor,
            "hostnames": host.hostnames,
        })
        if self.alert_mgr:
            try:
                await self.alert_mgr.send_alert(
                    title=f"Unknown device on network: {host.ip}",
                    body=(f"MAC: {host.mac} ({host.mac_vendor})\n"
                          f"Hostnames: {', '.join(host.hostnames) or 'none'}\n"
                          f"OS: {host.os_name or 'unknown'}"),
                    severity="high",
                )
            except Exception as e:
                log.warning("Failed to send rogue device alert: %s", e)
        if self._config.rogue_device_itsm_ticket and self.itsm:
            try:
                await self.itsm.create_ticket(
                    title=f"Unknown device detected: {host.ip}",
                    description=(
                        f"An unrecognized device appeared on the network.\n\n"
                        f"IP: {host.ip}\n"
                        f"MAC: {host.mac} ({host.mac_vendor})\n"
                        f"Hostnames: {', '.join(host.hostnames) or 'none'}\n"
                        f"OS fingerprint: {host.os_name or 'unknown'}\n\n"
                        f"Open ports: {', '.join(str(p.port) for p in host.ports)}"
                    ),
                    priority=self._config.rogue_device_itsm_priority,
                    source="network_scan",
                )
            except Exception as e:
                log.warning("Failed to create rogue device ticket: %s", e)

    # ── Finding handling ──────────────────────────────────────────────

    async def _handle_finding(self, finding: ScanFinding) -> None:
        """Create ITSM ticket for significant new findings."""
        if not self.itsm:
            return
        if finding.severity not in ("critical", "high"):
            return
        try:
            cve_str = ", ".join(finding.cve_ids) if finding.cve_ids else "none"
            await self.itsm.create_ticket(
                title=f"[{finding.severity.upper()}] {finding.title} on {finding.host}",
                description=(
                    f"Scanner: {finding.scanner}\n"
                    f"Host: {finding.host}"
                    + (f":{finding.port}" if finding.port else "") + "\n"
                    f"CVEs: {cve_str}\n"
                    f"CVSS: {finding.cvss_score or 'N/A'}\n\n"
                    f"{finding.description}\n\n"
                    f"Solution:\n{finding.solution}"
                ),
                priority=finding.severity,
                source="network_scan",
            )
        except Exception as e:
            log.warning("Failed to create finding ticket: %s", e)

    # ── Query interface ───────────────────────────────────────────────

    def list_hosts(self,
                   ip: str | None = None,
                   os_filter: str | None = None,
                   rogue_only: bool = False) -> list[DiscoveredHost]:
        hosts = list(self._hosts.values())
        if ip:
            hosts = [h for h in hosts if h.ip == ip]
        if os_filter:
            hosts = [h for h in hosts if os_filter.lower() in h.os_name.lower()]
        if rogue_only:
            hosts = [h for h in hosts if not h.in_itam]
        return sorted(hosts, key=lambda h: h.last_seen, reverse=True)

    def list_findings(self,
                      severity: str | None = None,
                      host: str | None = None,
                      scanner: str | None = None,
                      suppressed: bool | None = None) -> list[ScanFinding]:
        findings = list(self._findings.values())
        if severity:
            findings = [f for f in findings if f.severity == severity]
        if host:
            findings = [f for f in findings if f.host == host]
        if scanner:
            findings = [f for f in findings if f.scanner == scanner]
        if suppressed is not None:
            findings = [f for f in findings if f.suppressed == suppressed]
        return sorted(findings, key=lambda f: (
            _SEVERITIES.index(f.severity) if f.severity in _SEVERITIES else 99,
            -f.last_seen,
        ))

    def suppress_finding(self, finding_id: str) -> bool:
        f = self._findings.get(finding_id)
        if not f:
            return False
        f.suppressed = True
        self._save()
        return True

    def unsuppress_finding(self, finding_id: str) -> bool:
        f = self._findings.get(finding_id)
        if not f:
            return False
        f.suppressed = False
        self._save()
        return True

    def mark_host_in_itam(self, ip: str, node_id: str = "") -> bool:
        h = self._hosts.get(ip)
        if not h:
            return False
        h.in_itam = True
        h.node_id = node_id
        self._save()
        return True

    def list_results(self, limit: int = 20) -> list[ScanResult]:
        return sorted(self._results, key=lambda r: r.started_at, reverse=True)[:limit]

    def get_config(self) -> NetworkScanConfig:
        return self._config

    def set_config(self, config: NetworkScanConfig) -> None:
        self._config = config
        self._save()

    def status(self) -> dict[str, Any]:
        active_findings = [f for f in self._findings.values() if not f.suppressed]
        by_sev = {s: 0 for s in _SEVERITIES}
        for f in active_findings:
            by_sev[f.severity] = by_sev.get(f.severity, 0) + 1
        rogue = sum(1 for h in self._hosts.values() if not h.in_itam)
        scanners_available: dict[str, bool] = {}
        return {
            "hosts_known": len(self._hosts),
            "rogue_hosts": rogue,
            "findings": by_sev,
            "total_findings": len(active_findings),
            "suppressed_findings": sum(1 for f in self._findings.values() if f.suppressed),
            "last_discovery": self._last_discovery,
            "last_vuln_scan": self._last_vuln_scan,
            "targets": self._effective_targets(),
            "scanners_enabled": {
                "nmap": True,
                "nuclei": self._config.nuclei_enabled,
                "openvas": self._config.openvas_enabled,
                "nessus": self._config.nessus_enabled,
            },
        }

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, "data": data})
