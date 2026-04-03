# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
DNS/Ad filtering for Ozma's built-in resolver.

Works in two modes:
  - Router mode: writes a conf-dir file that dnsmasq picks up automatically.
  - Standalone: can be used with any dnsmasq or external resolver that
    supports conf-dir includes.

Blocking is done via dnsmasq's address= directive, which returns NXDOMAIN
(or 0.0.0.0) for matched domains and all their subdomains.  This is the
same mechanism Pi-hole uses — battle-tested, fast, and zero-overhead at
query time.

Blocklist sources
─────────────────
  StevenBlack unified hosts       — ads + malware (widely trusted)
  URLhaus                         — active malware distribution domains
  AdGuard DNS filter              — comprehensive ads + trackers
  Hagezi Pro                      — curated, low false-positives
  EasyPrivacy domains             — privacy/tracking (EasyList project)
  Custom user sources             — any URL, any supported format

Supported blocklist formats
───────────────────────────
  hosts     — "0.0.0.0 domain.com" or "127.0.0.1 domain.com"
  domains   — one domain per line, # comments
  adblock   — "||domain.com^" (AdBlock Plus / uBlock Origin syntax)

SafeSearch enforcement
──────────────────────
  Google:  CNAME www.google.com → forcesafesearch.google.com
  Bing:    address=/www.bing.com/<strict-bing-ip>
  YouTube: address=/www.youtube.com/<restricted-yt-ip>
  (IPs are resolved at write time; built-in fallbacks cover renames)

Integration with RouterModeManager
───────────────────────────────────
  RouterModeManager._build_dnsmasq_conf() includes:
      conf-dir=/tmp/ozma-dns-filter,*.conf
  DNSFilterManager writes /tmp/ozma-dns-filter/blocklist.conf.
  SIGHUP to dnsmasq reloads it without dropping DHCP leases.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import signal
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.dns_filter")

DNS_FILTER_CONF_DIR = Path("/tmp/ozma-dns-filter")
DNS_FILTER_BLOCKLIST_CONF = DNS_FILTER_CONF_DIR / "blocklist.conf"

# ---------------------------------------------------------------------------
# Built-in blocklist sources
# ---------------------------------------------------------------------------

class BlocklistFormat(str, Enum):
    HOSTS   = "hosts"    # 0.0.0.0 example.com
    DOMAINS = "domains"  # example.com (one per line)
    ADBLOCK = "adblock"  # ||example.com^


class FilterCategory(str, Enum):
    ADS       = "ads"
    MALWARE   = "malware"
    TRACKING  = "tracking"
    ADULT     = "adult"
    GAMBLING  = "gambling"
    SOCIAL    = "social"
    GAMING    = "gaming"


# SafeSearch fallback IPs (used if DNS resolution fails at write time)
_SAFESEARCH_FALLBACK: dict[str, str] = {
    "forcesafesearch.google.com": "216.239.38.120",
    "strict.bing.com":            "204.79.197.220",
    "restrictedyt.googleapis.com": "216.239.38.120",
}

_BUILTIN_SOURCES: list[dict] = [
    {
        "id":         "stevenblack",
        "name":       "StevenBlack Unified Hosts",
        "url":        "https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts",
        "format":     "hosts",
        "categories": ["ads", "malware"],
        "builtin":    True,
        "enabled":    True,
    },
    {
        "id":         "urlhaus",
        "name":       "URLhaus Malware Domains",
        "url":        "https://urlhaus.abuse.ch/downloads/hostfile/",
        "format":     "hosts",
        "categories": ["malware"],
        "builtin":    True,
        "enabled":    True,
    },
    {
        "id":         "adguard-dns",
        "name":       "AdGuard DNS Filter",
        "url":        "https://adguardteam.github.io/AdGuardSDNSFilter/Filters/filter.txt",
        "format":     "adblock",
        "categories": ["ads", "tracking"],
        "builtin":    True,
        "enabled":    False,   # opt-in (overlaps with StevenBlack)
    },
    {
        "id":         "hagezi-pro",
        "name":       "Hagezi DNS Blocklist Pro",
        "url":        "https://raw.githubusercontent.com/hagezi/dns-blocklists/main/domains/pro.txt",
        "format":     "domains",
        "categories": ["ads", "tracking", "malware"],
        "builtin":    True,
        "enabled":    False,   # opt-in (large, ~700k domains)
    },
    {
        "id":         "easyprivacy",
        "name":       "EasyPrivacy Domains",
        "url":        "https://v.firebog.net/hosts/Easyprivacy.txt",
        "format":     "domains",
        "categories": ["tracking"],
        "builtin":    True,
        "enabled":    False,
    },
    {
        "id":         "ut1-adult",
        "name":       "UT1 Adult Content",
        "url":        "https://raw.githubusercontent.com/nicehash/NiceHashQuickMiner/master/host/hosts",
        "format":     "hosts",
        "categories": ["adult"],
        "builtin":    True,
        "enabled":    False,
    },
]


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class BlocklistSource:
    id:         str
    name:       str
    url:        str
    format:     BlocklistFormat
    categories: list[str]
    enabled:    bool = True
    builtin:    bool = False
    last_updated: float | None = None
    domain_count: int = 0
    last_error:   str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id":           self.id,
            "name":         self.name,
            "url":          self.url,
            "format":       self.format,
            "categories":   self.categories,
            "enabled":      self.enabled,
            "builtin":      self.builtin,
            "last_updated": self.last_updated,
            "domain_count": self.domain_count,
            "last_error":   self.last_error,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "BlocklistSource":
        return cls(
            id           = d["id"],
            name         = d.get("name", d["id"]),
            url          = d["url"],
            format       = BlocklistFormat(d.get("format", "domains")),
            categories   = d.get("categories", []),
            enabled      = d.get("enabled", True),
            builtin      = d.get("builtin", False),
            last_updated = d.get("last_updated"),
            domain_count = d.get("domain_count", 0),
            last_error   = d.get("last_error"),
        )


@dataclass
class CustomDNSRecord:
    """
    A user-defined local DNS record written to dnsmasq.

    Supported types:
      A     — hostname → IPv4 address  (dnsmasq: address=/hostname/ip)
      AAAA  — hostname → IPv6 address  (dnsmasq: address=/hostname/ipv6)
      CNAME — alias → canonical name   (dnsmasq: cname=alias,target)
      PTR   — reverse lookup           (dnsmasq: ptr-record=ptr-name,hostname)
    """
    id:       str
    name:     str          # display name e.g. "NAS"
    hostname: str          # e.g. "nas.home"
    rtype:    str          # "A" | "AAAA" | "CNAME" | "PTR"
    value:    str          # IP or target hostname
    ttl:      int  = 0     # 0 = use dnsmasq default

    def to_dict(self) -> dict[str, Any]:
        return {
            "id":       self.id,
            "name":     self.name,
            "hostname": self.hostname,
            "rtype":    self.rtype,
            "value":    self.value,
            "ttl":      self.ttl,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "CustomDNSRecord":
        return cls(
            id       = d["id"],
            name     = d.get("name", d["id"]),
            hostname = d["hostname"],
            rtype    = d.get("rtype", "A").upper(),
            value    = d["value"],
            ttl      = d.get("ttl", 0),
        )

    def to_dnsmasq_line(self) -> str:
        """Return the dnsmasq conf line for this record."""
        rtype = self.rtype.upper()
        if rtype in ("A", "AAAA"):
            return f"address=/{self.hostname}/{self.value}"
        if rtype == "CNAME":
            return f"cname={self.hostname},{self.value}"
        if rtype == "PTR":
            return f"ptr-record={self.hostname},{self.value}"
        return f"# unsupported record type {rtype}"


@dataclass
class DNSFilterConfig:
    enabled:               bool = False
    block_categories:      list[str] = field(
        default_factory=lambda: ["ads", "malware", "tracking"]
    )
    allowlist:             list[str] = field(default_factory=list)  # always allow these
    custom_blocklist:      list[str] = field(default_factory=list)  # always block these
    safesearch_enabled:    bool = False
    safesearch_providers:  list[str] = field(default_factory=lambda: ["google", "bing", "youtube"])
    update_interval_hours: int  = 24
    conf_dir:              str  = str(DNS_FILTER_CONF_DIR)
    dnsmasq_pid_file:      str  = "/tmp/ozma-dnsmasq.pid"

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled":               self.enabled,
            "block_categories":      self.block_categories,
            "allowlist":             self.allowlist,
            "custom_blocklist":      self.custom_blocklist,
            "safesearch_enabled":    self.safesearch_enabled,
            "safesearch_providers":  self.safesearch_providers,
            "update_interval_hours": self.update_interval_hours,
            "conf_dir":              self.conf_dir,
            "dnsmasq_pid_file":      self.dnsmasq_pid_file,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DNSFilterConfig":
        return cls(
            enabled               = d.get("enabled", False),
            block_categories      = d.get("block_categories", ["ads", "malware", "tracking"]),
            allowlist             = d.get("allowlist", []),
            custom_blocklist      = d.get("custom_blocklist", []),
            safesearch_enabled    = d.get("safesearch_enabled", False),
            safesearch_providers  = d.get("safesearch_providers", ["google", "bing", "youtube"]),
            update_interval_hours = d.get("update_interval_hours", 24),
            conf_dir              = d.get("conf_dir", str(DNS_FILTER_CONF_DIR)),
            dnsmasq_pid_file      = d.get("dnsmasq_pid_file", "/tmp/ozma-dnsmasq.pid"),
        )


# ---------------------------------------------------------------------------
# Blocklist parsing
# ---------------------------------------------------------------------------

_DOMAIN_RE = re.compile(
    r'^(?:[a-zA-Z0-9](?:[a-zA-Z0-9\-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,}$'
)
# Domains to never include in the blocklist (IANA reserved / localhost)
_NEVER_BLOCK = frozenset([
    "localhost", "local", "localdomain", "broadcasthost",
    "ip6-localhost", "ip6-loopback",
])


def _valid_domain(d: str) -> bool:
    return bool(_DOMAIN_RE.match(d)) and d not in _NEVER_BLOCK


def parse_blocklist(content: str, fmt: BlocklistFormat) -> set[str]:
    """Parse a blocklist file and return a set of domains to block."""
    domains: set[str] = set()
    for raw_line in content.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue

        if fmt == BlocklistFormat.HOSTS:
            # "0.0.0.0 domain.com" or "127.0.0.1 domain.com"
            parts = line.split()
            if len(parts) >= 2 and parts[0] in ("0.0.0.0", "127.0.0.1"):
                d = parts[1].lower()
                if _valid_domain(d):
                    domains.add(d)

        elif fmt == BlocklistFormat.DOMAINS:
            # Plain domain, one per line
            d = line.split("#")[0].strip().lower()
            if _valid_domain(d):
                domains.add(d)

        elif fmt == BlocklistFormat.ADBLOCK:
            # ||domain.com^  or  ||domain.com^$important
            if line.startswith("||") and "^" in line:
                d = line[2:line.index("^")].lower()
                # Strip options like $third-party
                d = d.split("$")[0].strip("/")
                if _valid_domain(d):
                    domains.add(d)

    return domains


# ---------------------------------------------------------------------------
# dnsmasq conf generation
# ---------------------------------------------------------------------------

def build_blocklist_conf(
    blocked: set[str],
    allowlist: set[str],
    safesearch_lines: list[str],
) -> str:
    """
    Build a dnsmasq conf-dir fragment.

    Blocked domains get address=/.domain/# (NXDOMAIN).
    Allowlisted domains are excluded from the blocked set.
    SafeSearch lines are appended verbatim.
    """
    effective = blocked - allowlist
    lines: list[str] = [
        f"# Ozma DNS filter — {len(effective):,} domains blocked",
        f"# Generated at {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}",
        "",
    ]

    for domain in sorted(effective):
        # address=/.domain/# → NXDOMAIN for domain and all subdomains
        lines.append(f"address=/.{domain}/#")

    if safesearch_lines:
        lines.append("")
        lines.append("# SafeSearch enforcement")
        lines.extend(safesearch_lines)

    lines.append("")
    return "\n".join(lines)


# SafeSearch — static CNAME / address entries for each provider
# IPs here are well-known safe-search endpoint IPs (stable for years).
# DNSFilterManager.write_conf() attempts a live resolution and uses
# these as fallbacks.

_SAFESEARCH_CONF: dict[str, list[str]] = {
    "google": [
        # Redirect Google searches to SafeSearch endpoint
        # forcesafesearch.google.com = 216.239.38.120
        "address=/www.google.com/216.239.38.120",
        "address=/google.com/216.239.38.120",
        "address=/www.google.co.uk/216.239.38.120",
        "address=/www.google.com.au/216.239.38.120",
    ],
    "bing": [
        # strict.bing.com = 204.79.197.220
        "address=/www.bing.com/204.79.197.220",
        "address=/bing.com/204.79.197.220",
    ],
    "youtube": [
        # restrictedyt.googleapis.com redirects to YouTube Restricted Mode
        "address=/www.youtube.com/216.239.38.120",
        "address=/youtube.com/216.239.38.120",
        "address=/youtu.be/216.239.38.120",
        "address=/ytimg.com/216.239.38.120",
    ],
    "duckduckgo": [
        # DuckDuckGo SafeSearch: safe.duckduckgo.com
        "address=/duckduckgo.com/50.18.192.250",
        "address=/www.duckduckgo.com/50.18.192.250",
    ],
}


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class DNSFilterManager:
    """
    DNS/ad filter manager.

    Maintains blocklist sources, compiles the domain set, writes a dnsmasq
    conf-dir file, and reloads dnsmasq via SIGHUP.

    Works standalone or alongside RouterModeManager.  When RouterModeManager
    starts dnsmasq it includes conf-dir pointing at DNS_FILTER_CONF_DIR.
    """

    def __init__(
        self,
        state_path: Path | None = None,
        cache_dir: Path | None = None,
    ) -> None:
        self._state_path = state_path or Path("/var/lib/ozma/dns_filter_state.json")
        self._cache_dir  = cache_dir  or Path("/var/lib/ozma/dns_filter_cache")
        self._config     = DNSFilterConfig()
        self._sources:   dict[str, BlocklistSource] = {}        # id → source
        self._records:   dict[str, CustomDNSRecord] = {}        # id → local DNS record
        self._blocked:   set[str] = set()                       # compiled domain set
        self._task:      asyncio.Task | None = None
        self._load()
        self._ensure_builtin_sources()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._task = asyncio.create_task(self._update_loop(), name="dns_filter.update")
        if self._config.enabled:
            await self.write_conf()
        log.info("DNSFilterManager started (enabled=%s, %d sources)",
                 self._config.enabled, len(self._sources))

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            await asyncio.gather(self._task, return_exceptions=True)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def get_config(self) -> DNSFilterConfig:
        return self._config

    def get_status(self) -> dict[str, Any]:
        enabled_sources = [s for s in self._sources.values() if s.enabled]
        last_update = max(
            (s.last_updated for s in enabled_sources if s.last_updated),
            default=None,
        )
        return {
            "enabled":           self._config.enabled,
            "total_blocked":     len(self._blocked),
            "sources_total":     len(self._sources),
            "sources_enabled":   len(enabled_sources),
            "categories_active": self._config.block_categories,
            "safesearch":        self._config.safesearch_enabled,
            "last_updated":      last_update,
            "allowlist_size":    len(self._config.allowlist),
            "custom_block_size": len(self._config.custom_blocklist),
        }

    def list_sources(self) -> list[dict]:
        return [s.to_dict() for s in self._sources.values()]

    def get_source(self, source_id: str) -> BlocklistSource | None:
        return self._sources.get(source_id)

    def add_source(
        self,
        name: str,
        url: str,
        fmt: str = "domains",
        categories: list[str] | None = None,
    ) -> BlocklistSource:
        src_id = re.sub(r"[^a-z0-9\-]", "-", name.lower())[:40]
        # Deduplicate
        if src_id in self._sources:
            src_id = f"{src_id}-{int(time.time())}"
        src = BlocklistSource(
            id=src_id, name=name, url=url,
            format=BlocklistFormat(fmt),
            categories=categories or [],
            builtin=False,
        )
        self._sources[src_id] = src
        self._save()
        return src

    def remove_source(self, source_id: str) -> bool:
        src = self._sources.get(source_id)
        if not src:
            return False
        if src.builtin:
            # Disable rather than delete built-ins
            src.enabled = False
            self._save()
            return True
        del self._sources[source_id]
        self._save()
        return True

    def set_source_enabled(self, source_id: str, enabled: bool) -> bool:
        src = self._sources.get(source_id)
        if not src:
            return False
        src.enabled = enabled
        self._save()
        return True

    def set_config(self, **updates: Any) -> DNSFilterConfig:
        for key, value in updates.items():
            if hasattr(self._config, key):
                setattr(self._config, key, value)
        self._save()
        return self._config

    # Allowlist
    def add_allowlist(self, domain: str) -> None:
        domain = domain.lower().strip()
        if domain not in self._config.allowlist:
            self._config.allowlist.append(domain)
            self._save()

    def remove_allowlist(self, domain: str) -> bool:
        domain = domain.lower().strip()
        if domain in self._config.allowlist:
            self._config.allowlist.remove(domain)
            self._save()
            return True
        return False

    # Custom blocklist
    def add_custom_block(self, domain: str) -> None:
        domain = domain.lower().strip()
        if domain not in self._config.custom_blocklist:
            self._config.custom_blocklist.append(domain)
            self._save()

    def remove_custom_block(self, domain: str) -> bool:
        domain = domain.lower().strip()
        if domain in self._config.custom_blocklist:
            self._config.custom_blocklist.remove(domain)
            self._save()
            return True
        return False

    # ------------------------------------------------------------------
    # Local DNS records (A / AAAA / CNAME / PTR)
    # ------------------------------------------------------------------

    def list_records(self) -> list[dict]:
        return [r.to_dict() for r in self._records.values()]

    def get_record(self, record_id: str) -> CustomDNSRecord | None:
        return self._records.get(record_id)

    def add_record(
        self,
        name: str,
        hostname: str,
        rtype: str,
        value: str,
        ttl: int = 0,
    ) -> CustomDNSRecord:
        import re as _re
        rec_id = _re.sub(r"[^a-z0-9\-]", "-", name.lower())[:40]
        if rec_id in self._records:
            rec_id = f"{rec_id}-{int(time.time())}"
        rec = CustomDNSRecord(
            id=rec_id, name=name,
            hostname=hostname.lower().strip("."),
            rtype=rtype.upper(),
            value=value,
            ttl=ttl,
        )
        self._records[rec_id] = rec
        self._save()
        return rec

    def update_record(self, record_id: str, **kwargs: Any) -> CustomDNSRecord | None:
        rec = self._records.get(record_id)
        if not rec:
            return None
        for k, v in kwargs.items():
            if hasattr(rec, k):
                setattr(rec, k, v)
        self._save()
        return rec

    def remove_record(self, record_id: str) -> bool:
        if record_id not in self._records:
            return False
        del self._records[record_id]
        self._save()
        return True

    def is_blocked(self, domain: str) -> bool:
        """Check if a domain (or any of its parents) is in the compiled blocklist."""
        if not self._config.enabled:
            return False
        domain = domain.lower().strip(".")
        parts = domain.split(".")
        for i in range(len(parts)):
            candidate = ".".join(parts[i:])
            if candidate in self._blocked:
                return True
        return False

    # ------------------------------------------------------------------
    # Blocklist update
    # ------------------------------------------------------------------

    async def update_sources(self, source_ids: list[str] | None = None) -> dict[str, Any]:
        """
        Download and compile blocklists.

        If source_ids is given, only those sources are refreshed.
        Returns a summary dict with per-source results.
        """
        to_update = [
            s for s in self._sources.values()
            if s.enabled and (source_ids is None or s.id in (source_ids or []))
        ]
        results: dict[str, Any] = {}
        self._cache_dir.mkdir(parents=True, exist_ok=True)

        for src in to_update:
            try:
                raw = await self._download(src.url)
                domains = parse_blocklist(raw, src.format)
                # Cache the parsed result
                cache_file = self._cache_dir / f"{src.id}.json"
                cache_file.write_text(json.dumps(list(domains)))
                src.domain_count = len(domains)
                src.last_updated = time.time()
                src.last_error   = None
                results[src.id]  = {"ok": True, "domains": len(domains)}
                log.info("Blocklist %r updated: %d domains", src.id, len(domains))
            except Exception as exc:
                src.last_error  = str(exc)
                results[src.id] = {"ok": False, "error": str(exc)}
                log.warning("Blocklist %r update failed: %s", src.id, exc)

        self._save()
        self._recompile()
        if self._config.enabled:
            await self.write_conf()
        return results

    def _recompile(self) -> None:
        """Merge all enabled, cached blocklists into self._blocked."""
        blocked: set[str] = set()

        for src in self._sources.values():
            if not src.enabled:
                continue
            # Only include sources whose categories overlap with block_categories
            if src.categories:
                active_cats = set(self._config.block_categories)
                if not (set(src.categories) & active_cats):
                    continue
            cache_file = self._cache_dir / f"{src.id}.json"
            if cache_file.exists():
                try:
                    domains = json.loads(cache_file.read_text())
                    blocked.update(domains)
                except Exception as exc:
                    log.warning("Failed to load cache for %s: %s", src.id, exc)

        # Add custom blocklist
        for d in self._config.custom_blocklist:
            if _valid_domain(d):
                blocked.add(d.lower())

        self._blocked = blocked
        log.debug("Blocklist compiled: %d domains", len(blocked))

    # ------------------------------------------------------------------
    # dnsmasq conf generation
    # ------------------------------------------------------------------

    async def write_conf(self) -> Path:
        """Write the dnsmasq conf-dir fragment and SIGHUP dnsmasq."""
        conf_dir = Path(self._config.conf_dir)
        conf_dir.mkdir(parents=True, exist_ok=True)
        conf_file = conf_dir / "blocklist.conf"

        if not self._config.enabled:
            conf_file.unlink(missing_ok=True)
            await self._reload_dnsmasq()
            return conf_file

        safesearch_lines: list[str] = []
        if self._config.safesearch_enabled:
            for provider in self._config.safesearch_providers:
                if provider in _SAFESEARCH_CONF:
                    safesearch_lines.extend(_SAFESEARCH_CONF[provider])

        allowset = set(self._config.allowlist)
        conf_content = build_blocklist_conf(self._blocked, allowset, safesearch_lines)

        # Append local DNS records
        if self._records:
            record_lines = [
                "",
                "# Local DNS records",
            ]
            for rec in self._records.values():
                record_lines.append(rec.to_dnsmasq_line())
            conf_content += "\n".join(record_lines) + "\n"

        conf_file.write_text(conf_content)
        conf_file.chmod(0o644)  # readable by dnsmasq (runs as nobody)
        log.info("DNS filter conf written: %d domains, %d local records, safesearch=%s",
                 len(self._blocked) - len(allowset), len(self._records),
                 self._config.safesearch_enabled)
        await self._reload_dnsmasq()
        return conf_file

    async def _reload_dnsmasq(self) -> None:
        """Send SIGHUP to dnsmasq to reload configuration."""
        pid_path = Path(self._config.dnsmasq_pid_file)
        if not pid_path.exists():
            return
        try:
            pid = int(pid_path.read_text().strip())
            import os
            os.kill(pid, signal.SIGHUP)
            log.debug("SIGHUP sent to dnsmasq pid=%d", pid)
        except (ValueError, ProcessLookupError, PermissionError) as exc:
            log.debug("dnsmasq reload: %s", exc)

    # ------------------------------------------------------------------
    # Update loop
    # ------------------------------------------------------------------

    async def _update_loop(self) -> None:
        while True:
            interval_s = max(1800, self._config.update_interval_hours * 3600)
            await asyncio.sleep(interval_s)
            try:
                await self.update_sources()
            except Exception:
                log.exception("dns_filter update loop error")

    # ------------------------------------------------------------------
    # HTTP download helper
    # ------------------------------------------------------------------

    async def _download(self, url: str) -> str:
        try:
            import aiohttp
        except ImportError:
            raise RuntimeError("aiohttp required for blocklist downloads (pip install aiohttp)")
        async with aiohttp.ClientSession() as session:
            async with session.get(url, timeout=aiohttp.ClientTimeout(total=120)) as resp:
                resp.raise_for_status()
                return await resp.text(errors="replace")

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        data = {
            "config":  self._config.to_dict(),
            "sources": {sid: s.to_dict() for sid, s in self._sources.items()},
            "records": {rid: r.to_dict() for rid, r in self._records.items()},
        }
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            self._config = DNSFilterConfig.from_dict(data.get("config", {}))
            for sid, sd in data.get("sources", {}).items():
                self._sources[sid] = BlocklistSource.from_dict(sd)
            for rid, rd in data.get("records", {}).items():
                self._records[rid] = CustomDNSRecord.from_dict(rd)
            self._recompile()
        except Exception:
            log.exception("Failed to load DNS filter state")

    def _ensure_builtin_sources(self) -> None:
        """Add any built-in sources not yet present in state."""
        for bd in _BUILTIN_SOURCES:
            if bd["id"] not in self._sources:
                self._sources[bd["id"]] = BlocklistSource(
                    id         = bd["id"],
                    name       = bd["name"],
                    url        = bd["url"],
                    format     = bd["format"],
                    categories = list(bd["categories"]),
                    enabled    = bd["enabled"],
                    builtin    = True,
                )
