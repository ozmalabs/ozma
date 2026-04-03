#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for NetworkScanManager — device discovery and vulnerability assessment.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from network_scan import (
    NetworkScanManager, NetworkScanConfig, OpenVASConfig, NessusConfig,
    DiscoveredHost, ScanFinding, ScanResult, OpenPort,
    NmapScanner, NucleiScanner,
    _parse_nmap_xml, _parse_nuclei_jsonl, _parse_openvas_xml,
    _cvss_to_severity, _SEVERITIES,
)


def _mgr(tmp: Path, config: NetworkScanConfig | None = None) -> NetworkScanManager:
    return NetworkScanManager(tmp, config=config)


def _host(**kwargs) -> DiscoveredHost:
    defaults = dict(
        ip="192.168.1.10", mac="aa:bb:cc:dd:ee:ff", mac_vendor="Acme",
        hostnames=["myhost.local"], os_name="Linux 5.x", os_accuracy=90,
        ports=[], last_seen=time.time(), first_seen=time.time(),
    )
    defaults.update(kwargs)
    return DiscoveredHost(**defaults)


def _finding(**kwargs) -> ScanFinding:
    defaults = dict(
        id="f1", scanner="nmap", host="192.168.1.10",
        title="Test finding", severity="high", cvss_score=7.5,
        cve_ids=["CVE-2024-1234"], first_seen=time.time(), last_seen=time.time(),
    )
    defaults.update(kwargs)
    return ScanFinding(**defaults)


def _run(coro):
    return asyncio.get_event_loop().run_until_complete(coro)


# ── Model serialization ───────────────────────────────────────────────────────

class TestDiscoveredHostModel(unittest.TestCase):
    def test_roundtrip(self):
        port = OpenPort(port=22, protocol="tcp", service="ssh", version="OpenSSH 9.3")
        h = _host(ports=[port])
        h2 = DiscoveredHost.from_dict(h.to_dict())
        self.assertEqual(h2.ip, h.ip)
        self.assertEqual(len(h2.ports), 1)
        self.assertEqual(h2.ports[0].port, 22)
        self.assertEqual(h2.ports[0].service, "ssh")

    def test_defaults_for_missing_fields(self):
        h = DiscoveredHost.from_dict({"ip": "10.0.0.1"})
        self.assertEqual(h.mac, "")
        self.assertFalse(h.in_itam)

    def test_to_dict_includes_ports(self):
        h = _host(ports=[OpenPort(port=443, service="https")])
        d = h.to_dict()
        self.assertEqual(len(d["ports"]), 1)
        self.assertEqual(d["ports"][0]["port"], 443)


class TestScanFindingModel(unittest.TestCase):
    def test_roundtrip(self):
        f = _finding()
        f2 = ScanFinding.from_dict(f.to_dict())
        self.assertEqual(f2.id, f.id)
        self.assertEqual(f2.cve_ids, f.cve_ids)
        self.assertEqual(f2.severity, f.severity)

    def test_defaults(self):
        f = ScanFinding.from_dict({
            "id": "x", "scanner": "nmap", "host": "1.2.3.4",
            "title": "Test", "severity": "info",
        })
        self.assertFalse(f.suppressed)
        self.assertEqual(f.cve_ids, [])
        self.assertIsNone(f.port)


class TestOpenPortModel(unittest.TestCase):
    def test_roundtrip(self):
        p = OpenPort(port=8080, protocol="tcp", service="http-alt", version="nginx 1.25")
        p2 = OpenPort.from_dict(p.to_dict())
        self.assertEqual(p2.port, 8080)
        self.assertEqual(p2.version, "nginx 1.25")


# ── Config serialization ──────────────────────────────────────────────────────

class TestNetworkScanConfig(unittest.TestCase):
    def test_defaults(self):
        cfg = NetworkScanConfig()
        self.assertEqual(cfg.discovery_interval, 3600)
        self.assertEqual(cfg.vuln_scan_interval, 86400)
        self.assertTrue(cfg.rogue_device_alert)
        self.assertTrue(cfg.nuclei_enabled)
        self.assertFalse(cfg.openvas_enabled)
        self.assertFalse(cfg.nessus_enabled)

    def test_roundtrip(self):
        cfg = NetworkScanConfig(
            targets=["192.168.1.0/24", "10.0.0.0/24"],
            discovery_interval=1800,
            nuclei_enabled=True,
            nuclei_severity="critical,high",
            openvas_enabled=True,
            nessus_enabled=True,
            ticket_severity_threshold="critical",
        )
        cfg.openvas.host = "openvas.local"
        cfg.openvas.port = 9390
        cfg.nessus.url = "https://nessus.local:8834"
        cfg.nessus.access_key_env = "NESSUS_ACCESS"
        d = cfg.to_dict()
        cfg2 = NetworkScanConfig.from_dict(d)
        self.assertEqual(cfg2.targets, ["192.168.1.0/24", "10.0.0.0/24"])
        self.assertEqual(cfg2.discovery_interval, 1800)
        self.assertEqual(cfg2.openvas.host, "openvas.local")
        self.assertEqual(cfg2.nessus.url, "https://nessus.local:8834")
        self.assertEqual(cfg2.nessus.access_key_env, "NESSUS_ACCESS")


# ── Severity mapping ──────────────────────────────────────────────────────────

class TestCvssToSeverity(unittest.TestCase):
    def test_critical(self):
        self.assertEqual(_cvss_to_severity(9.5), "critical")
        self.assertEqual(_cvss_to_severity(9.0), "critical")

    def test_high(self):
        self.assertEqual(_cvss_to_severity(8.9), "high")
        self.assertEqual(_cvss_to_severity(7.0), "high")

    def test_medium(self):
        self.assertEqual(_cvss_to_severity(6.9), "medium")
        self.assertEqual(_cvss_to_severity(4.0), "medium")

    def test_low(self):
        self.assertEqual(_cvss_to_severity(3.9), "low")
        self.assertEqual(_cvss_to_severity(0.1), "low")

    def test_info(self):
        self.assertEqual(_cvss_to_severity(0.0), "info")


# ── Nmap XML parsing ──────────────────────────────────────────────────────────

_NMAP_XML_SIMPLE = """<?xml version="1.0"?>
<nmaprun>
  <host>
    <status state="up"/>
    <address addrtype="ipv4" addr="192.168.1.5"/>
    <address addrtype="mac" addr="11:22:33:44:55:66" vendor="Intel"/>
    <hostnames>
      <hostname name="desktop.local" type="PTR"/>
    </hostnames>
    <os>
      <osmatch name="Linux 5.15" accuracy="95"/>
    </os>
    <ports>
      <port portid="22" protocol="tcp">
        <state state="open"/>
        <service name="ssh" product="OpenSSH" version="9.3p1" tunnel=""/>
      </port>
      <port portid="80" protocol="tcp">
        <state state="open"/>
        <service name="http" product="nginx" version="1.25.0"/>
      </port>
      <port portid="9999" protocol="tcp">
        <state state="closed"/>
        <service name="unknown"/>
      </port>
    </ports>
  </host>
  <host>
    <status state="down"/>
    <address addrtype="ipv4" addr="192.168.1.6"/>
  </host>
</nmaprun>"""

_NMAP_XML_WITH_VULN = """<?xml version="1.0"?>
<nmaprun>
  <host>
    <status state="up"/>
    <address addrtype="ipv4" addr="192.168.1.10"/>
    <ports>
      <port portid="445" protocol="tcp">
        <state state="open"/>
        <service name="smb"/>
        <script id="smb-vuln-ms17-010" output="VULNERABLE: CVE-2017-0144&#10;State: VULNERABLE&#10;IDs: CVE:CVE-2017-0144">
          <elem key="state">VULNERABLE</elem>
          <elem key="cvss">9.3</elem>
        </script>
      </port>
    </ports>
  </host>
</nmaprun>"""


class TestParseNmapXml(unittest.TestCase):
    def test_parses_up_hosts_only(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        ips = [h.ip for h in hosts]
        self.assertIn("192.168.1.5", ips)
        self.assertNotIn("192.168.1.6", ips)

    def test_parses_mac_and_vendor(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        h = next(h for h in hosts if h.ip == "192.168.1.5")
        self.assertEqual(h.mac, "11:22:33:44:55:66")
        self.assertEqual(h.mac_vendor, "Intel")

    def test_parses_hostname(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        h = next(h for h in hosts if h.ip == "192.168.1.5")
        self.assertIn("desktop.local", h.hostnames)

    def test_parses_os(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        h = next(h for h in hosts if h.ip == "192.168.1.5")
        self.assertEqual(h.os_name, "Linux 5.15")
        self.assertEqual(h.os_accuracy, 95)

    def test_only_open_ports(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        h = next(h for h in hosts if h.ip == "192.168.1.5")
        port_numbers = [p.port for p in h.ports]
        self.assertIn(22, port_numbers)
        self.assertIn(80, port_numbers)
        self.assertNotIn(9999, port_numbers)  # closed port

    def test_service_version_combined(self):
        hosts, _ = _parse_nmap_xml(_NMAP_XML_SIMPLE)
        h = next(h for h in hosts if h.ip == "192.168.1.5")
        ssh = next(p for p in h.ports if p.port == 22)
        self.assertEqual(ssh.service, "ssh")
        self.assertIn("OpenSSH", ssh.version)

    def test_vuln_script_produces_finding(self):
        hosts, findings = _parse_nmap_xml(_NMAP_XML_WITH_VULN)
        self.assertTrue(len(findings) > 0)
        f = findings[0]
        self.assertEqual(f.scanner, "nmap")
        self.assertEqual(f.host, "192.168.1.10")
        self.assertEqual(f.port, 445)

    def test_vuln_script_cvss_severity(self):
        _, findings = _parse_nmap_xml(_NMAP_XML_WITH_VULN)
        f = findings[0]
        self.assertAlmostEqual(f.cvss_score, 9.3)
        self.assertEqual(f.severity, "critical")

    def test_vuln_cve_extracted(self):
        _, findings = _parse_nmap_xml(_NMAP_XML_WITH_VULN)
        f = findings[0]
        self.assertIn("CVE-2017-0144", f.cve_ids)

    def test_invalid_xml_returns_empty(self):
        hosts, findings = _parse_nmap_xml("{not xml}")
        self.assertEqual(hosts, [])
        self.assertEqual(findings, [])


# ── Nuclei JSONL parsing ──────────────────────────────────────────────────────

_NUCLEI_JSONL = """\
{"template-id":"CVE-2021-41773","host":"http://192.168.1.20:80","matched-at":"http://192.168.1.20:80/cgi-bin/.%2e/.%2e/bin/sh","info":{"name":"Apache HTTP Server 2.4.49 - Path Traversal","severity":"critical","description":"Path traversal vuln","remediation":"Upgrade Apache","classification":{"cve-id":["CVE-2021-41773"],"cvss-score":7.5}}}
{"template-id":"detect-springboot-actuator","host":"http://192.168.1.25:8080","matched-at":"http://192.168.1.25:8080/actuator","info":{"name":"Spring Boot Actuator Exposed","severity":"medium","description":"Actuator endpoint is exposed","classification":{}}}
not-json-line
"""


class TestParseNucleiJsonl(unittest.TestCase):
    def test_parses_valid_lines(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        self.assertEqual(len(findings), 2)

    def test_skips_invalid_json(self):
        # "not-json-line" is skipped
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        self.assertEqual(len(findings), 2)

    def test_critical_severity(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        f = next(f for f in findings if "41773" in f.plugin_id)
        self.assertEqual(f.severity, "critical")
        self.assertIn("CVE-2021-41773", f.cve_ids)

    def test_medium_severity(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        f = next(f for f in findings if "actuator" in f.plugin_id)
        self.assertEqual(f.severity, "medium")

    def test_host_ip_extracted(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        ips = {f.host for f in findings}
        self.assertIn("192.168.1.20", ips)
        self.assertIn("192.168.1.25", ips)

    def test_port_extracted(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        f = next(f for f in findings if "actuator" in f.plugin_id)
        self.assertEqual(f.port, 8080)

    def test_scanner_field(self):
        findings = _parse_nuclei_jsonl(_NUCLEI_JSONL)
        for f in findings:
            self.assertEqual(f.scanner, "nuclei")


# ── OpenVAS XML parsing ───────────────────────────────────────────────────────

_OPENVAS_XML = """<?xml version="1.0"?>
<get_results_response status="200">
  <result id="result-001">
    <host>192.168.1.30</host>
    <port>443/tcp</port>
    <name>TLS Weak Cipher Suites</name>
    <threat>medium</threat>
    <description>The server supports weak TLS cipher suites.</description>
    <solution>Disable weak ciphers in the TLS configuration.</solution>
    <nvt oid="1.3.6.1.4.1.25623.1.0.1234">
      <refs>
        <ref type="cve" id="CVE-2013-2566"/>
      </refs>
      <cvss_base>5.0</cvss_base>
    </nvt>
  </result>
  <result id="result-002">
    <host>192.168.1.31</host>
    <port>22/tcp</port>
    <name>SSH Outdated Version</name>
    <threat>high</threat>
    <description>SSH version is outdated and vulnerable.</description>
    <solution>Upgrade to OpenSSH 9.x.</solution>
    <nvt oid="1.3.6.1.4.1.25623.1.0.5678">
      <refs/>
      <cvss_base>7.8</cvss_base>
    </nvt>
  </result>
</get_results_response>"""


class TestParseOpenVASXml(unittest.TestCase):
    def test_parses_results(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        self.assertEqual(len(findings), 2)

    def test_scanner_field(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        for f in findings:
            self.assertEqual(f.scanner, "openvas")

    def test_host_field(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        ips = {f.host for f in findings}
        self.assertIn("192.168.1.30", ips)
        self.assertIn("192.168.1.31", ips)

    def test_severity_from_cvss(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        f30 = next(f for f in findings if f.host == "192.168.1.30")
        self.assertAlmostEqual(f30.cvss_score, 5.0)
        self.assertEqual(f30.severity, "medium")

    def test_high_severity(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        f31 = next(f for f in findings if f.host == "192.168.1.31")
        self.assertEqual(f31.severity, "high")

    def test_cve_extraction(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        f30 = next(f for f in findings if f.host == "192.168.1.30")
        self.assertIn("CVE-2013-2566", f30.cve_ids)

    def test_port_extraction(self):
        findings = _parse_openvas_xml(_OPENVAS_XML, time.time())
        f30 = next(f for f in findings if f.host == "192.168.1.30")
        self.assertEqual(f30.port, 443)

    def test_invalid_xml_returns_empty(self):
        findings = _parse_openvas_xml("{invalid", time.time())
        self.assertEqual(findings, [])


# ── NetworkScanManager — no scanners (pure logic) ────────────────────────────

class TestNetworkScanManagerBasic(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        self._mgr = _mgr(self._path)

    def test_list_hosts_empty(self):
        self.assertEqual(self._mgr.list_hosts(), [])

    def test_list_findings_empty(self):
        self.assertEqual(self._mgr.list_findings(), [])

    def test_list_results_empty(self):
        self.assertEqual(self._mgr.list_results(), [])

    def test_status_structure(self):
        s = self._mgr.status()
        self.assertIn("hosts_known", s)
        self.assertIn("rogue_hosts", s)
        self.assertIn("findings", s)
        self.assertIn("scanners_enabled", s)
        self.assertIn("nmap", s["scanners_enabled"])
        self.assertIn("openvas", s["scanners_enabled"])
        self.assertIn("nessus", s["scanners_enabled"])

    def test_get_config_returns_config(self):
        cfg = self._mgr.get_config()
        self.assertIsInstance(cfg, NetworkScanConfig)

    def test_set_config_persists(self):
        cfg = NetworkScanConfig(targets=["10.0.0.0/8"], discovery_interval=7200)
        self._mgr.set_config(cfg)
        mgr2 = _mgr(self._path)
        self.assertEqual(mgr2.get_config().targets, ["10.0.0.0/8"])
        self.assertEqual(mgr2.get_config().discovery_interval, 7200)


# ── Host management ───────────────────────────────────────────────────────────

class TestHostManagement(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        self._mgr = _mgr(self._path)
        self._mgr._hosts = {
            "192.168.1.1": _host(ip="192.168.1.1", in_itam=True, node_id="node-a",
                                   os_name="Linux 5.x"),
            "192.168.1.2": _host(ip="192.168.1.2", in_itam=False, os_name="Windows 11"),
            "192.168.1.3": _host(ip="192.168.1.3", in_itam=False, os_name="macOS 14"),
        }

    def test_list_all(self):
        self.assertEqual(len(self._mgr.list_hosts()), 3)

    def test_filter_by_ip(self):
        hosts = self._mgr.list_hosts(ip="192.168.1.2")
        self.assertEqual(len(hosts), 1)
        self.assertEqual(hosts[0].ip, "192.168.1.2")

    def test_filter_rogue_only(self):
        hosts = self._mgr.list_hosts(rogue_only=True)
        self.assertEqual(len(hosts), 2)
        for h in hosts:
            self.assertFalse(h.in_itam)

    def test_filter_by_os(self):
        hosts = self._mgr.list_hosts(os_filter="Windows")
        self.assertEqual(len(hosts), 1)
        self.assertEqual(hosts[0].ip, "192.168.1.2")

    def test_filter_os_case_insensitive(self):
        hosts = self._mgr.list_hosts(os_filter="linux")
        self.assertEqual(len(hosts), 1)

    def test_mark_host_in_itam(self):
        ok = self._mgr.mark_host_in_itam("192.168.1.2", node_id="node-b")
        self.assertTrue(ok)
        h = self._mgr.list_hosts(ip="192.168.1.2")[0]
        self.assertTrue(h.in_itam)
        self.assertEqual(h.node_id, "node-b")

    def test_mark_unknown_host_returns_false(self):
        ok = self._mgr.mark_host_in_itam("10.255.255.255")
        self.assertFalse(ok)

    def test_rogue_count_in_status(self):
        s = self._mgr.status()
        self.assertEqual(s["rogue_hosts"], 2)

    def test_hosts_persist(self):
        self._mgr._save()
        mgr2 = _mgr(self._path)
        self.assertEqual(len(mgr2.list_hosts()), 3)


# ── Finding management ────────────────────────────────────────────────────────

class TestFindingManagement(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        self._mgr = _mgr(self._path)
        self._mgr._findings = {
            "f-crit": _finding(id="f-crit", severity="critical", host="10.0.0.1",
                                scanner="nessus"),
            "f-high": _finding(id="f-high", severity="high", host="10.0.0.2",
                                scanner="nuclei"),
            "f-med": _finding(id="f-med", severity="medium", host="10.0.0.1",
                               scanner="openvas"),
            "f-suppressed": _finding(id="f-suppressed", severity="high",
                                      host="10.0.0.3", suppressed=True),
        }

    def test_list_all(self):
        findings = self._mgr.list_findings()
        self.assertEqual(len(findings), 4)

    def test_filter_by_severity(self):
        findings = self._mgr.list_findings(severity="critical")
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].id, "f-crit")

    def test_filter_by_host(self):
        findings = self._mgr.list_findings(host="10.0.0.1")
        self.assertEqual(len(findings), 2)

    def test_filter_by_scanner(self):
        findings = self._mgr.list_findings(scanner="nuclei")
        self.assertEqual(len(findings), 1)

    def test_filter_suppressed_false(self):
        findings = self._mgr.list_findings(suppressed=False)
        self.assertEqual(len(findings), 3)
        for f in findings:
            self.assertFalse(f.suppressed)

    def test_filter_suppressed_true(self):
        findings = self._mgr.list_findings(suppressed=True)
        self.assertEqual(len(findings), 1)

    def test_suppress_finding(self):
        ok = self._mgr.suppress_finding("f-high")
        self.assertTrue(ok)
        self.assertTrue(self._mgr._findings["f-high"].suppressed)

    def test_unsuppress_finding(self):
        ok = self._mgr.unsuppress_finding("f-suppressed")
        self.assertTrue(ok)
        self.assertFalse(self._mgr._findings["f-suppressed"].suppressed)

    def test_suppress_unknown_returns_false(self):
        ok = self._mgr.suppress_finding("no-such-id")
        self.assertFalse(ok)

    def test_findings_sorted_by_severity(self):
        findings = self._mgr.list_findings(suppressed=False)
        sev_order = [_SEVERITIES.index(f.severity) for f in findings
                     if f.severity in _SEVERITIES]
        self.assertEqual(sev_order, sorted(sev_order))

    def test_findings_persist(self):
        self._mgr._save()
        mgr2 = _mgr(self._path)
        self.assertEqual(len(mgr2.list_findings()), 4)

    def test_status_findings_count(self):
        s = self._mgr.status()
        self.assertEqual(s["findings"]["critical"], 1)
        self.assertEqual(s["findings"]["high"], 1)  # suppressed one not counted
        self.assertEqual(s["findings"]["medium"], 1)

    def test_suppressed_count_in_status(self):
        s = self._mgr.status()
        self.assertEqual(s["suppressed_findings"], 1)


# ── Discovery scan (mocked nmap) ──────────────────────────────────────────────

class TestDiscoveryScan(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        cfg = NetworkScanConfig(targets=["192.168.1.0/24"],
                                 rogue_device_alert=False,
                                 rogue_device_itsm_ticket=False)
        self._mgr = _mgr(self._path, config=cfg)

    def test_discovery_adds_new_hosts(self):
        fresh = [_host(ip="192.168.1.50"), _host(ip="192.168.1.51")]
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.discovery_scan = AsyncMock(return_value=fresh)
            MockNmap.return_value = mock_scanner
            result = _run(self._mgr.run_discovery())
        self.assertEqual(result["new_hosts"], 2)
        self.assertEqual(result["hosts_found"], 2)
        self.assertEqual(len(self._mgr.list_hosts()), 2)

    def test_discovery_preserves_itam_flag(self):
        # Host already known + marked
        self._mgr._hosts["192.168.1.50"] = _host(ip="192.168.1.50",
                                                    in_itam=True, node_id="node-x")
        fresh = [_host(ip="192.168.1.50")]
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.discovery_scan = AsyncMock(return_value=fresh)
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_discovery())
        h = self._mgr.list_hosts(ip="192.168.1.50")[0]
        self.assertTrue(h.in_itam)
        self.assertEqual(h.node_id, "node-x")

    def test_discovery_updates_last_discovery_timestamp(self):
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.discovery_scan = AsyncMock(return_value=[])
            MockNmap.return_value = mock_scanner
            before = time.time()
            _run(self._mgr.run_discovery())
        self.assertGreaterEqual(self._mgr._last_discovery, before)

    def test_discovery_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.discovery_scan = AsyncMock(return_value=[])
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_discovery())
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("network_scan.discovery.complete", types)

    def test_discovery_no_targets_returns_error(self):
        mgr = _mgr(self._path / "sub", config=NetworkScanConfig(targets=[]))
        # Without auto-detect falling back, this may still try to detect subnets
        # But we can test with a config that has no subnet-detection fallback
        # by mocking _effective_targets to return empty
        with patch.object(mgr, "_effective_targets", return_value=[]):
            result = _run(mgr.run_discovery())
        self.assertFalse(result["ok"])

    def test_discovery_nmap_unavailable_returns_error(self):
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=False)
            MockNmap.return_value = mock_scanner
            result = _run(self._mgr.run_discovery())
        self.assertFalse(result.get("ok", True))


# ── Rogue device handling ─────────────────────────────────────────────────────

class TestRogueDeviceHandling(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        cfg = NetworkScanConfig(
            targets=["192.168.1.0/24"],
            rogue_device_alert=True,
            rogue_device_itsm_ticket=True,
            rogue_device_itsm_priority="high",
        )
        self._mgr = _mgr(self._path, config=cfg)
        self._mock_itsm = AsyncMock()
        self._mock_itsm.create_ticket = AsyncMock()
        self._mgr.itsm = self._mock_itsm

    def test_rogue_device_creates_itsm_ticket(self):
        host = _host(ip="192.168.1.99")
        _run(self._mgr._handle_rogue_device(host))
        self._mock_itsm.create_ticket.assert_called_once()
        call_kwargs = self._mock_itsm.create_ticket.call_args[1]
        self.assertEqual(call_kwargs.get("priority"), "high")
        self.assertIn("192.168.1.99", call_kwargs.get("title", ""))

    def test_rogue_device_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        host = _host(ip="10.0.0.99", mac="de:ad:be:ef:00:01", mac_vendor="Unknown")
        _run(self._mgr._handle_rogue_device(host))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("network_scan.rogue_device", types)
        ev = next(e for e in events if e["type"] == "network_scan.rogue_device")
        self.assertEqual(ev["data"]["ip"], "10.0.0.99")

    def test_rogue_device_no_ticket_when_disabled(self):
        self._mgr._config.rogue_device_itsm_ticket = False
        host = _host(ip="192.168.1.100")
        _run(self._mgr._handle_rogue_device(host))
        self._mock_itsm.create_ticket.assert_not_called()


# ── Vuln scan (mocked scanners) ───────────────────────────────────────────────

class TestVulnScan(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        cfg = NetworkScanConfig(
            targets=["10.0.0.0/24"],
            nuclei_enabled=False,
            openvas_enabled=False,
            nessus_enabled=False,
            ticket_severity_threshold="critical",
        )
        self._mgr = _mgr(self._path, config=cfg)

    def test_vuln_scan_with_nmap_only(self):
        nmap_host = _host(ip="10.0.0.5")
        nmap_finding = _finding(id="nf1", host="10.0.0.5", severity="high")
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([nmap_host], [nmap_finding]))
            MockNmap.return_value = mock_scanner
            result = _run(self._mgr.run_vuln_scan())
        self.assertIn("nmap", result["scanners_used"])
        self.assertIsNotNone(self._mgr._findings.get("nf1"))

    def test_vuln_scan_merges_findings_preserving_suppressed(self):
        # Pre-existing suppressed finding
        existing = _finding(id="nf1", suppressed=True)
        self._mgr._findings["nf1"] = existing
        nmap_finding = _finding(id="nf1", host="10.0.0.5", severity="high")
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([_host()], [nmap_finding]))
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_vuln_scan())
        # Suppressed flag preserved after merge
        self.assertTrue(self._mgr._findings["nf1"].suppressed)

    def test_vuln_scan_first_seen_preserved(self):
        old_time = time.time() - 3600
        existing = _finding(id="nf1", first_seen=old_time)
        self._mgr._findings["nf1"] = existing
        nmap_finding = _finding(id="nf1", first_seen=time.time())
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([_host()], [nmap_finding]))
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_vuln_scan())
        self.assertAlmostEqual(self._mgr._findings["nf1"].first_seen, old_time, delta=1)

    def test_vuln_scan_creates_ticket_for_critical(self):
        mock_itsm = AsyncMock()
        mock_itsm.create_ticket = AsyncMock()
        self._mgr.itsm = mock_itsm
        crit_finding = _finding(id="crit1", severity="critical")
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([_host()], [crit_finding]))
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_vuln_scan())
        mock_itsm.create_ticket.assert_called()

    def test_vuln_scan_no_ticket_below_threshold(self):
        # threshold = critical; high finding should not create ticket
        mock_itsm = AsyncMock()
        mock_itsm.create_ticket = AsyncMock()
        self._mgr.itsm = mock_itsm
        high_finding = _finding(id="high1", severity="high")
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([_host()], [high_finding]))
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_vuln_scan())
        mock_itsm.create_ticket.assert_not_called()

    def test_vuln_scan_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([], []))
            MockNmap.return_value = mock_scanner
            _run(self._mgr.run_vuln_scan())
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("network_scan.vuln_scan.complete", types)

    def test_vuln_scan_updates_last_vuln_scan_timestamp(self):
        with patch("network_scan.NmapScanner") as MockNmap:
            mock_scanner = AsyncMock()
            mock_scanner.available = AsyncMock(return_value=True)
            mock_scanner.scan = AsyncMock(return_value=([], []))
            MockNmap.return_value = mock_scanner
            before = time.time()
            _run(self._mgr.run_vuln_scan())
        self.assertGreaterEqual(self._mgr._last_vuln_scan, before)


# ── Persistence ───────────────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)

    def test_hosts_and_findings_persist(self):
        mgr = _mgr(self._path)
        mgr._hosts["10.0.0.1"] = _host(ip="10.0.0.1", in_itam=True)
        mgr._findings["f1"] = _finding(id="f1", severity="critical")
        mgr._save()
        mgr2 = _mgr(self._path)
        self.assertIsNotNone(mgr2._hosts.get("10.0.0.1"))
        self.assertIsNotNone(mgr2._findings.get("f1"))
        self.assertTrue(mgr2._hosts["10.0.0.1"].in_itam)

    def test_scan_results_persist(self):
        mgr = _mgr(self._path)
        mgr._results.append(ScanResult(
            id="r1", started_at=time.time(), finished_at=time.time(),
            targets=["10.0.0.0/24"], hosts_found=5, scanners_used=["nmap"],
        ))
        mgr._save()
        mgr2 = _mgr(self._path)
        self.assertEqual(len(mgr2.list_results()), 1)

    def test_corrupt_config_uses_defaults(self):
        cfg_path = self._path / "network_scan.json"
        self._path.mkdir(parents=True, exist_ok=True)
        cfg_path.write_text("{bad json")
        mgr = _mgr(self._path)
        self.assertEqual(mgr.get_config().discovery_interval, 3600)

    def test_corrupt_hosts_uses_empty(self):
        self._path.mkdir(parents=True, exist_ok=True)
        (self._path / "hosts.json").write_text("{bad")
        mgr = _mgr(self._path)
        self.assertEqual(mgr.list_hosts(), [])

    def test_timestamps_persist(self):
        mgr = _mgr(self._path)
        mgr._last_discovery = 1000.0
        mgr._last_vuln_scan = 2000.0
        mgr._save()
        mgr2 = _mgr(self._path)
        self.assertAlmostEqual(mgr2._last_discovery, 1000.0, delta=1)
        self.assertAlmostEqual(mgr2._last_vuln_scan, 2000.0, delta=1)


# ── Lifecycle ─────────────────────────────────────────────────────────────────

class TestLifecycle(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)

    def test_start_creates_task(self):
        mgr = _mgr(self._path)
        _run(mgr.start())
        self.assertEqual(len(mgr._tasks), 1)
        _run(mgr.stop())

    def test_stop_cancels_tasks(self):
        mgr = _mgr(self._path)
        _run(mgr.start())
        _run(mgr.stop())
        for t in mgr._tasks:
            self.assertTrue(t.cancelled() or t.done())


if __name__ == "__main__":
    unittest.main()
