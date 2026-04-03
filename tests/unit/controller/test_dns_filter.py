# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for DNSFilterManager and blocklist parsing."""

import json
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import sys

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from dns_filter import (
    BlocklistFormat,
    BlocklistSource,
    DNSFilterConfig,
    DNSFilterManager,
    FilterCategory,
    _valid_domain,
    build_blocklist_conf,
    parse_blocklist,
)


# ---------------------------------------------------------------------------
# Domain validation
# ---------------------------------------------------------------------------

class TestValidDomain:
    def test_simple_domain(self):
        assert _valid_domain("example.com")

    def test_subdomain(self):
        assert _valid_domain("sub.example.com")

    def test_localhost_rejected(self):
        assert not _valid_domain("localhost")

    def test_local_rejected(self):
        assert not _valid_domain("local")

    def test_ip_rejected(self):
        assert not _valid_domain("192.168.1.1")

    def test_single_label_rejected(self):
        assert not _valid_domain("noextension")

    def test_empty_rejected(self):
        assert not _valid_domain("")

    def test_broadcasthost_rejected(self):
        assert not _valid_domain("broadcasthost")


# ---------------------------------------------------------------------------
# Blocklist parsing — hosts format
# ---------------------------------------------------------------------------

class TestParseHosts:
    SAMPLE = """
# Comment line
127.0.0.1 localhost
0.0.0.0 doubleclick.net
0.0.0.0 ads.example.com
127.0.0.1 tracker.evil.com

# Another comment
0.0.0.0 malware.domain.co.uk
::1 ip6only.com
"""

    def test_extracts_domains(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.HOSTS)
        assert "doubleclick.net" in result
        assert "ads.example.com" in result
        assert "tracker.evil.com" in result
        assert "malware.domain.co.uk" in result

    def test_excludes_localhost(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.HOSTS)
        assert "localhost" not in result

    def test_excludes_comments(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.HOSTS)
        # No raw comment lines
        for d in result:
            assert not d.startswith("#")

    def test_ipv6_line_included(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.HOSTS)
        # ::1 lines are not parsed (only 0.0.0.0 / 127.0.0.1)
        assert "ip6only.com" not in result


# ---------------------------------------------------------------------------
# Blocklist parsing — domains format
# ---------------------------------------------------------------------------

class TestParseDomains:
    SAMPLE = """
# Tracking domains
tracker.example.com
ads.google-analytics.com

# Malware
phishing-site.net
malware.badactor.org
invalid  # no TLD
192.168.1.1  # IP, not domain
"""

    def test_extracts_valid_domains(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.DOMAINS)
        assert "tracker.example.com" in result
        assert "ads.google-analytics.com" in result
        assert "phishing-site.net" in result
        assert "malware.badactor.org" in result

    def test_excludes_invalid(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.DOMAINS)
        assert "invalid" not in result
        assert "192.168.1.1" not in result

    def test_inline_comments_stripped(self):
        result = parse_blocklist("valid.com # this is a comment\n", BlocklistFormat.DOMAINS)
        assert "valid.com" in result


# ---------------------------------------------------------------------------
# Blocklist parsing — adblock format
# ---------------------------------------------------------------------------

class TestParseAdblock:
    SAMPLE = """
! AdBlock Plus filter list
||doubleclick.net^
||ads.example.com^$third-party
||tracker.evil.com^
/some/path/rule
@@||allowed.com^
example.com (no pipe prefix)
"""

    def test_extracts_pipe_domains(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.ADBLOCK)
        assert "doubleclick.net" in result
        assert "ads.example.com" in result
        assert "tracker.evil.com" in result

    def test_excludes_exception_rules(self):
        # @@|| exception rules don't start with ||
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.ADBLOCK)
        assert "allowed.com" not in result

    def test_excludes_path_rules(self):
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.ADBLOCK)
        for d in result:
            assert not d.startswith("/")

    def test_options_stripped(self):
        # $third-party should be stripped
        result = parse_blocklist(self.SAMPLE, BlocklistFormat.ADBLOCK)
        for d in result:
            assert "$" not in d


# ---------------------------------------------------------------------------
# build_blocklist_conf
# ---------------------------------------------------------------------------

class TestBuildBlocklistConf:
    def test_produces_address_lines(self):
        blocked = {"evil.com", "tracker.net"}
        conf = build_blocklist_conf(blocked, set(), [])
        assert "address=/.evil.com/#" in conf
        assert "address=/.tracker.net/#" in conf

    def test_allowlist_excluded(self):
        blocked = {"evil.com", "safe-ads.example.com"}
        allowed = {"safe-ads.example.com"}
        conf = build_blocklist_conf(blocked, allowed, [])
        assert "safe-ads.example.com" not in conf
        assert "evil.com" in conf

    def test_safesearch_lines_included(self):
        conf = build_blocklist_conf(set(), set(), ["address=/www.google.com/216.239.38.120"])
        assert "address=/www.google.com/216.239.38.120" in conf

    def test_comment_with_domain_count(self):
        conf = build_blocklist_conf({"a.com", "b.com"}, set(), [])
        assert "2 domains blocked" in conf

    def test_empty_blocklist(self):
        conf = build_blocklist_conf(set(), set(), [])
        assert "0 domains blocked" in conf


# ---------------------------------------------------------------------------
# BlocklistSource model
# ---------------------------------------------------------------------------

class TestBlocklistSourceModel:
    def test_roundtrip(self):
        src = BlocklistSource(
            id="test", name="Test", url="http://example.com/list.txt",
            format=BlocklistFormat.DOMAINS, categories=["ads"],
            enabled=True, builtin=False,
        )
        d = src.to_dict()
        restored = BlocklistSource.from_dict(d)
        assert restored.id == src.id
        assert restored.format == BlocklistFormat.DOMAINS
        assert restored.categories == ["ads"]

    def test_last_updated_preserved(self):
        ts = time.time()
        src = BlocklistSource(
            id="x", name="X", url="http://x.com", format=BlocklistFormat.HOSTS,
            categories=[], last_updated=ts,
        )
        d = src.to_dict()
        restored = BlocklistSource.from_dict(d)
        assert restored.last_updated == ts


# ---------------------------------------------------------------------------
# DNSFilterConfig model
# ---------------------------------------------------------------------------

class TestDNSFilterConfig:
    def test_defaults(self):
        cfg = DNSFilterConfig()
        assert not cfg.enabled
        assert "ads" in cfg.block_categories
        assert "malware" in cfg.block_categories

    def test_roundtrip(self):
        cfg = DNSFilterConfig(
            enabled=True,
            block_categories=["ads", "tracking"],
            allowlist=["safe.com"],
            custom_blocklist=["bad.com"],
            safesearch_enabled=True,
            safesearch_providers=["google"],
        )
        d = cfg.to_dict()
        restored = DNSFilterConfig.from_dict(d)
        assert restored.enabled
        assert restored.allowlist == ["safe.com"]
        assert restored.custom_blocklist == ["bad.com"]
        assert restored.safesearch_enabled
        assert restored.safesearch_providers == ["google"]


# ---------------------------------------------------------------------------
# DNSFilterManager — CRUD
# ---------------------------------------------------------------------------

def _manager(tmp_path: Path) -> DNSFilterManager:
    return DNSFilterManager(
        state_path=tmp_path / "state.json",
        cache_dir=tmp_path / "cache",
    )


class TestDNSFilterManagerCRUD:
    def test_builtin_sources_present(self, tmp_path):
        mgr = _manager(tmp_path)
        sources = {s["id"] for s in mgr.list_sources()}
        assert "stevenblack" in sources
        assert "urlhaus" in sources

    def test_add_custom_source(self, tmp_path):
        mgr = _manager(tmp_path)
        src = mgr.add_source("My List", "http://example.com/list.txt", "domains", ["ads"])
        assert src.id in {s["id"] for s in mgr.list_sources()}

    def test_remove_custom_source(self, tmp_path):
        mgr = _manager(tmp_path)
        src = mgr.add_source("Temp", "http://x.com/list", "domains")
        ok = mgr.remove_source(src.id)
        assert ok
        assert src.id not in {s["id"] for s in mgr.list_sources()}

    def test_remove_builtin_disables_not_deletes(self, tmp_path):
        mgr = _manager(tmp_path)
        ok = mgr.remove_source("stevenblack")
        assert ok
        src = mgr.get_source("stevenblack")
        assert src is not None
        assert not src.enabled

    def test_set_source_enabled(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_source_enabled("urlhaus", False)
        assert not mgr.get_source("urlhaus").enabled

    def test_set_source_enabled_missing(self, tmp_path):
        mgr = _manager(tmp_path)
        assert not mgr.set_source_enabled("does-not-exist", True)

    def test_add_allowlist(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_allowlist("safe.example.com")
        assert "safe.example.com" in mgr.get_config().allowlist

    def test_remove_allowlist(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_allowlist("safe.example.com")
        ok = mgr.remove_allowlist("safe.example.com")
        assert ok
        assert "safe.example.com" not in mgr.get_config().allowlist

    def test_remove_allowlist_missing(self, tmp_path):
        mgr = _manager(tmp_path)
        assert not mgr.remove_allowlist("nothere.com")

    def test_add_custom_block(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_custom_block("bad.example.com")
        assert "bad.example.com" in mgr.get_config().custom_blocklist

    def test_remove_custom_block(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_custom_block("bad.example.com")
        ok = mgr.remove_custom_block("bad.example.com")
        assert ok
        assert "bad.example.com" not in mgr.get_config().custom_blocklist

    def test_set_config(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, safesearch_enabled=True)
        assert mgr.get_config().enabled
        assert mgr.get_config().safesearch_enabled


# ---------------------------------------------------------------------------
# DNSFilterManager — compilation
# ---------------------------------------------------------------------------

class TestDNSFilterManagerCompilation:
    def _write_cache(self, tmp_path: Path, source_id: str, domains: list[str]) -> None:
        cache_dir = tmp_path / "cache"
        cache_dir.mkdir(parents=True, exist_ok=True)
        (cache_dir / f"{source_id}.json").write_text(json.dumps(domains))

    def test_recompile_includes_enabled_source(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["ads.example.com", "track.evil.com"])
        mgr._recompile()
        assert "ads.example.com" in mgr._blocked

    def test_recompile_excludes_disabled_source(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["ads.example.com"])
        mgr.set_source_enabled("stevenblack", False)
        mgr._recompile()
        assert "ads.example.com" not in mgr._blocked

    def test_recompile_includes_custom_block(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_custom_block("custom-evil.com")
        mgr._recompile()
        assert "custom-evil.com" in mgr._blocked

    def test_recompile_category_filter(self, tmp_path):
        mgr = _manager(tmp_path)
        # Only ads category enabled, tracking source not included
        mgr.set_config(block_categories=["ads"])
        # adguard-dns is tracking+ads — should still be included
        # easyprivacy is tracking only — should be excluded
        self._write_cache(tmp_path, "easyprivacy", ["track.only.com"])
        mgr._recompile()
        # easyprivacy categories = ["tracking"], not in ["ads"]
        assert "track.only.com" not in mgr._blocked


# ---------------------------------------------------------------------------
# DNSFilterManager — is_blocked
# ---------------------------------------------------------------------------

class TestIsBlocked:
    def _write_cache(self, tmp_path: Path, source_id: str, domains: list[str]) -> None:
        cache_dir = tmp_path / "cache"
        cache_dir.mkdir(parents=True, exist_ok=True)
        (cache_dir / f"{source_id}.json").write_text(json.dumps(domains))

    def test_blocked_domain_returns_true(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["evil.com"])
        mgr.set_config(enabled=True)
        mgr._recompile()
        assert mgr.is_blocked("evil.com")

    def test_subdomain_of_blocked_blocked(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["evil.com"])
        mgr.set_config(enabled=True)
        mgr._recompile()
        assert mgr.is_blocked("sub.evil.com")

    def test_unblocked_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["evil.com"])
        mgr.set_config(enabled=True)
        mgr._recompile()
        assert not mgr.is_blocked("safe.com")

    def test_filter_disabled_returns_false(self, tmp_path):
        mgr = _manager(tmp_path)
        self._write_cache(tmp_path, "stevenblack", ["evil.com"])
        mgr.set_config(enabled=False)
        mgr._recompile()
        assert not mgr.is_blocked("evil.com")


# ---------------------------------------------------------------------------
# DNSFilterManager — persistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_config_survives_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, safesearch_enabled=True)
        mgr.add_allowlist("kept.com")
        mgr.add_custom_block("blocked.com")

        mgr2 = _manager(tmp_path)
        assert mgr2.get_config().enabled
        assert mgr2.get_config().safesearch_enabled
        assert "kept.com" in mgr2.get_config().allowlist
        assert "blocked.com" in mgr2.get_config().custom_blocklist

    def test_custom_sources_survive_reload(self, tmp_path):
        mgr = _manager(tmp_path)
        src = mgr.add_source("Custom", "http://x.com/list", "domains", ["ads"])

        mgr2 = _manager(tmp_path)
        assert src.id in {s["id"] for s in mgr2.list_sources()}

    def test_state_file_is_mode_600(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True)
        state_file = tmp_path / "state.json"
        assert oct(state_file.stat().st_mode)[-3:] == "600"


# ---------------------------------------------------------------------------
# DNSFilterManager — write_conf
# ---------------------------------------------------------------------------

class TestWriteConf:
    @pytest.mark.asyncio
    async def test_write_conf_creates_file(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))
        cache_dir = tmp_path / "cache"
        cache_dir.mkdir()
        (cache_dir / "stevenblack.json").write_text(json.dumps(["blocked.com"]))
        mgr._recompile()

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        assert conf_file.exists()
        content = conf_file.read_text()
        assert "address=/.blocked.com/#" in content

    @pytest.mark.asyncio
    async def test_write_conf_disabled_removes_file(self, tmp_path):
        conf_dir = tmp_path / "dnsmasq-conf"
        conf_dir.mkdir()
        conf_file = conf_dir / "blocklist.conf"
        conf_file.write_text("old content")

        mgr = _manager(tmp_path)
        mgr.set_config(enabled=False, conf_dir=str(conf_dir))

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            await mgr.write_conf()

        assert not conf_file.exists()

    @pytest.mark.asyncio
    async def test_safesearch_lines_in_conf(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(
            enabled=True,
            safesearch_enabled=True,
            safesearch_providers=["google"],
            conf_dir=str(tmp_path / "dnsmasq-conf"),
        )

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "SafeSearch" in content
        assert "216.239.38.120" in content  # Google SafeSearch IP

    @pytest.mark.asyncio
    async def test_allowlist_domain_excluded_from_conf(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_custom_block("blocked.com")
        mgr.add_custom_block("allowed-but-blocked.com")
        mgr.add_allowlist("allowed-but-blocked.com")
        mgr._recompile()
        mgr.set_config(enabled=True, conf_dir=str(tmp_path / "dnsmasq-conf"))

        with patch.object(mgr, "_reload_dnsmasq", AsyncMock()):
            conf_file = await mgr.write_conf()

        content = conf_file.read_text()
        assert "allowed-but-blocked.com" not in content
        assert "blocked.com" in content


# ---------------------------------------------------------------------------
# DNSFilterManager — status
# ---------------------------------------------------------------------------

class TestStatus:
    def test_status_fields(self, tmp_path):
        mgr = _manager(tmp_path)
        status = mgr.get_status()
        assert "enabled" in status
        assert "total_blocked" in status
        assert "sources_total" in status
        assert "sources_enabled" in status
        assert "categories_active" in status
        assert "safesearch" in status
        assert "last_updated" in status

    def test_status_reflects_blocked_count(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.add_custom_block("a.com")
        mgr.add_custom_block("b.com")
        mgr._recompile()
        assert mgr.get_status()["total_blocked"] == 2


# ---------------------------------------------------------------------------
# DNSFilterManager — lifecycle
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestLifecycle:
    async def test_start_stop(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=False)  # no conf write needed
        with patch.object(mgr, "write_conf", AsyncMock()):
            await mgr.start()
            assert mgr._task is not None
            await mgr.stop()
            assert mgr._task.cancelled() or mgr._task.done()

    async def test_start_enabled_writes_conf(self, tmp_path):
        mgr = _manager(tmp_path)
        mgr.set_config(enabled=True)
        with patch.object(mgr, "write_conf", AsyncMock()) as mock_write:
            await mgr.start()
            await mgr.stop()
        mock_write.assert_called_once()
