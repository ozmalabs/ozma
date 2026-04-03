# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for CustomDNSRecord and DNS record CRUD in DNSFilterManager."""

import json
import sys
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from dns_filter import CustomDNSRecord, DNSFilterManager


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> DNSFilterManager:
    return DNSFilterManager(
        state_path=tmp_path / "dns_filter_state.json",
        cache_dir=tmp_path / "cache",
    )


# ---------------------------------------------------------------------------
# TestCustomDNSRecord
# ---------------------------------------------------------------------------

class TestCustomDNSRecord:
    def test_to_dict_has_all_keys(self):
        rec = CustomDNSRecord(
            id="nas", name="NAS", hostname="nas.home", rtype="A", value="192.168.1.10",
        )
        d = rec.to_dict()
        for key in ("id", "name", "hostname", "rtype", "value", "ttl"):
            assert key in d

    def test_roundtrip(self):
        rec = CustomDNSRecord(
            id="nas", name="NAS", hostname="nas.home",
            rtype="A", value="192.168.1.10", ttl=300,
        )
        restored = CustomDNSRecord.from_dict(rec.to_dict())
        assert restored.id == "nas"
        assert restored.name == "NAS"
        assert restored.hostname == "nas.home"
        assert restored.rtype == "A"
        assert restored.value == "192.168.1.10"
        assert restored.ttl == 300

    def test_from_dict_defaults(self):
        rec = CustomDNSRecord.from_dict({
            "id": "x", "hostname": "x.home", "rtype": "A", "value": "10.0.0.1",
        })
        assert rec.name == "x"   # falls back to id
        assert rec.ttl == 0

    def test_from_dict_rtype_uppercased(self):
        rec = CustomDNSRecord.from_dict({
            "id": "y", "hostname": "y.home", "rtype": "a", "value": "10.0.0.2",
        })
        assert rec.rtype == "A"

    def test_a_record_dnsmasq_line(self):
        rec = CustomDNSRecord(
            id="nas", name="NAS", hostname="nas.home", rtype="A", value="192.168.1.10",
        )
        line = rec.to_dnsmasq_line()
        assert line == "address=/nas.home/192.168.1.10"

    def test_aaaa_record_dnsmasq_line(self):
        rec = CustomDNSRecord(
            id="nas6", name="NAS6", hostname="nas.home",
            rtype="AAAA", value="fd00::1",
        )
        line = rec.to_dnsmasq_line()
        assert line == "address=/nas.home/fd00::1"

    def test_cname_record_dnsmasq_line(self):
        rec = CustomDNSRecord(
            id="www", name="WWW", hostname="www.home",
            rtype="CNAME", value="server.home",
        )
        line = rec.to_dnsmasq_line()
        assert line == "cname=www.home,server.home"

    def test_ptr_record_dnsmasq_line(self):
        rec = CustomDNSRecord(
            id="ptr1", name="PTR1",
            hostname="10.1.168.192.in-addr.arpa",
            rtype="PTR", value="nas.home",
        )
        line = rec.to_dnsmasq_line()
        assert line == "ptr-record=10.1.168.192.in-addr.arpa,nas.home"

    def test_unsupported_type_produces_comment(self):
        rec = CustomDNSRecord(
            id="mx", name="MX", hostname="example.home",
            rtype="MX", value="mail.home",
        )
        line = rec.to_dnsmasq_line()
        assert line.startswith("#")


# ---------------------------------------------------------------------------
# TestDNSFilterManagerRecords
# ---------------------------------------------------------------------------

class TestDNSFilterManagerRecords:
    def test_add_record_returns_custom_dns_record(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        assert isinstance(rec, CustomDNSRecord)
        assert rec.name == "NAS"
        assert rec.hostname == "nas.home"
        assert rec.rtype == "A"
        assert rec.value == "192.168.1.10"

    def test_add_record_generates_id_from_name(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("NAS Device", "nas.home", "A", "192.168.1.10")
        assert rec.id == "nas-device"

    def test_add_record_hostname_lowercased(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("Test", "NAS.Home", "A", "10.0.0.1")
        assert rec.hostname == "nas.home"

    def test_update_record_changes_value(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        updated = mgr.update_record(rec.id, value="192.168.1.20")
        assert updated is not None
        assert updated.value == "192.168.1.20"

    def test_update_record_missing_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        result = mgr.update_record("nonexistent", value="10.0.0.1")
        assert result is None

    def test_remove_record_returns_true(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("Del", "del.home", "A", "10.0.0.1")
        assert mgr.remove_record(rec.id) is True
        assert mgr.get_record(rec.id) is None

    def test_remove_record_missing_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.remove_record("ghost") is False

    def test_list_records_returns_list_of_dicts(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        mgr.add_record("Server", "server.home", "CNAME", "nas.home")
        records = mgr.list_records()
        assert isinstance(records, list)
        assert len(records) == 2
        for r in records:
            assert isinstance(r, dict)
            assert "id" in r

    def test_get_record_by_id(self, tmp_path):
        mgr = _manager(tmp_path)
        rec = mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        found = mgr.get_record(rec.id)
        assert found is not None
        assert found.hostname == "nas.home"

    def test_get_record_missing_returns_none(self, tmp_path):
        mgr = _manager(tmp_path)
        assert mgr.get_record("not-here") is None

    def test_records_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        mgr.add_record("WWW", "www.home", "CNAME", "nas.home")

        mgr2 = _manager(tmp_path)
        records = mgr2.list_records()
        assert len(records) == 2
        hostnames = {r["hostname"] for r in records}
        assert "nas.home" in hostnames
        assert "www.home" in hostnames

    def test_record_ttl_preserved_on_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10", ttl=600)

        mgr2 = _manager(tmp_path)
        records = mgr2.list_records()
        assert records[0]["ttl"] == 600


# ---------------------------------------------------------------------------
# TestWriteConfWithRecords
# ---------------------------------------------------------------------------

class TestWriteConfWithRecords:
    @pytest.mark.asyncio
    async def test_write_conf_includes_a_record_line(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "address=/nas.home/192.168.1.10" in content

    @pytest.mark.asyncio
    async def test_write_conf_includes_cname_line(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        mgr.add_record("WWW", "www.home", "CNAME", "nas.home")

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "cname=www.home,nas.home" in content

    @pytest.mark.asyncio
    async def test_write_conf_multiple_records_all_present(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")
        mgr.add_record("Camera", "cam.home", "A", "192.168.1.20")
        mgr.add_record("Proxy", "proxy.home", "CNAME", "nas.home")

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "address=/nas.home/192.168.1.10" in content
        assert "address=/cam.home/192.168.1.20" in content
        assert "cname=proxy.home,nas.home" in content

    @pytest.mark.asyncio
    async def test_write_conf_records_section_labeled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        mgr.add_record("NAS", "nas.home", "A", "192.168.1.10")

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "Local DNS records" in content

    @pytest.mark.asyncio
    async def test_write_conf_no_records_section_when_empty(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        # No records added

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "Local DNS records" not in content

    @pytest.mark.asyncio
    async def test_write_conf_ptr_record_present(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        mgr.add_record(
            "PTR NAS",
            "10.1.168.192.in-addr.arpa",
            "PTR",
            "nas.home",
        )

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "ptr-record=10.1.168.192.in-addr.arpa,nas.home" in content
