# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Dynamic DNS update client for the Ozma home system controller.

Supports: Cloudflare, Namecheap, DuckDNS, No-IP, Dynu, Gandi LiveDNS.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.ddns")

_IPV4_RE = re.compile(r"^\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}$")

try:
    import aiohttp as aiohttp
except ImportError:
    aiohttp = None  # type: ignore


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class DDNSRecord:
    id: str
    name: str
    provider: str          # cloudflare | namecheap | duckdns | noip | dynu | gandi
    credentials: dict
    hostnames: list[str]
    ipv4: bool = True
    ipv6: bool = False
    last_ip: str | None = None
    last_ipv6: str | None = None
    last_updated: float | None = None
    last_error: str | None = None
    enabled: bool = True

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "provider": self.provider,
            "credentials": self.credentials,
            "hostnames": self.hostnames,
            "ipv4": self.ipv4,
            "ipv6": self.ipv6,
            "last_ip": self.last_ip,
            "last_ipv6": self.last_ipv6,
            "last_updated": self.last_updated,
            "last_error": self.last_error,
            "enabled": self.enabled,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DDNSRecord":
        return cls(
            id=d["id"],
            name=d["name"],
            provider=d["provider"],
            credentials=d["credentials"],
            hostnames=d["hostnames"],
            ipv4=d.get("ipv4", True),
            ipv6=d.get("ipv6", False),
            last_ip=d.get("last_ip"),
            last_ipv6=d.get("last_ipv6"),
            last_updated=d.get("last_updated"),
            last_error=d.get("last_error"),
            enabled=d.get("enabled", True),
        )


@dataclass
class DDNSConfig:
    enabled: bool = False
    check_interval_seconds: int = 300
    ip_providers: list[str] = field(default_factory=lambda: [
        "https://api.ipify.org",
        "https://icanhazip.com",
        "https://api4.my-ip.io/ip",
    ])
    ipv6_providers: list[str] = field(default_factory=lambda: [
        "https://api6.ipify.org",
        "https://icanhazip.com",
    ])

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "check_interval_seconds": self.check_interval_seconds,
            "ip_providers": self.ip_providers,
            "ipv6_providers": self.ipv6_providers,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DDNSConfig":
        return cls(
            enabled=d.get("enabled", False),
            check_interval_seconds=d.get("check_interval_seconds", 300),
            ip_providers=d.get("ip_providers", cls.__dataclass_fields__["ip_providers"].default_factory()),
            ipv6_providers=d.get("ipv6_providers", cls.__dataclass_fields__["ipv6_providers"].default_factory()),
        )


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class DDNSManager:
    STATE_PATH = Path("/var/lib/ozma/ddns_state.json")

    def __init__(self, state_path: Path | None = None) -> None:
        self._state_path = state_path or self.STATE_PATH
        self._config = DDNSConfig()
        self._records: dict[str, DDNSRecord] = {}
        self._task: asyncio.Task | None = None
        self._current_ipv4: str | None = None
        self._current_ipv6: str | None = None
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        if not self._config.enabled:
            log.info("ddns: disabled, not starting check loop")
            return
        if self._task and not self._task.done():
            return
        self._task = asyncio.create_task(self._check_loop(), name="ddns.check_loop")
        log.info("ddns: started (interval=%ds)", self._config.check_interval_seconds)

    async def stop(self) -> None:
        if self._task and not self._task.done():
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        self._task = None
        log.info("ddns: stopped")

    # ------------------------------------------------------------------
    # Record CRUD
    # ------------------------------------------------------------------

    def add_record(
        self,
        name: str,
        provider: str,
        credentials: dict,
        hostnames: list[str],
        **kwargs,
    ) -> DDNSRecord:
        record = DDNSRecord(
            id=str(uuid.uuid4())[:8],
            name=name,
            provider=provider,
            credentials=credentials,
            hostnames=hostnames,
            ipv4=kwargs.get("ipv4", True),
            ipv6=kwargs.get("ipv6", False),
            enabled=kwargs.get("enabled", True),
        )
        self._records[record.id] = record
        self._save()
        log.info("ddns: added record %s (%s / %s)", record.id, name, provider)
        return record

    def update_record(self, record_id: str, **kwargs) -> DDNSRecord | None:
        record = self._records.get(record_id)
        if record is None:
            return None
        for key, value in kwargs.items():
            if hasattr(record, key):
                setattr(record, key, value)
        self._save()
        log.info("ddns: updated record %s", record_id)
        return record

    def remove_record(self, record_id: str) -> bool:
        if record_id not in self._records:
            return False
        del self._records[record_id]
        self._save()
        log.info("ddns: removed record %s", record_id)
        return True

    def list_records(self) -> list[dict]:
        return [r.to_dict() for r in self._records.values()]

    def get_record(self, record_id: str) -> DDNSRecord | None:
        return self._records.get(record_id)

    # ------------------------------------------------------------------
    # Config
    # ------------------------------------------------------------------

    def get_config(self) -> DDNSConfig:
        return self._config

    def set_config(self, **kwargs) -> DDNSConfig:
        restart_needed = False
        for key, value in kwargs.items():
            if hasattr(self._config, key):
                if key == "enabled" and value != self._config.enabled:
                    restart_needed = True
                setattr(self._config, key, value)
        self._save()
        if restart_needed:
            asyncio.create_task(self._handle_config_change(), name="ddns.config_change")
        return self._config

    async def _handle_config_change(self) -> None:
        await self.stop()
        if self._config.enabled:
            await self.start()

    def get_status(self) -> dict:
        enabled_count = sum(1 for r in self._records.values() if r.enabled)
        return {
            "enabled": self._config.enabled,
            "records_total": len(self._records),
            "records_enabled": enabled_count,
            "current_ipv4": self._current_ipv4,
            "current_ipv6": self._current_ipv6,
            "records": [r.to_dict() for r in self._records.values()],
        }

    # ------------------------------------------------------------------
    # Public update trigger
    # ------------------------------------------------------------------

    async def update_now(self, record_id: str | None = None) -> dict[str, Any]:
        ip = await self._get_current_ip()
        needs_ipv6 = any(r.ipv6 for r in self._records.values() if r.enabled)
        ipv6 = await self._get_current_ipv6() if needs_ipv6 else None

        results: dict[str, Any] = {}

        if record_id is not None:
            record = self._records.get(record_id)
            if record is None:
                return {record_id: {"ok": False, "ip": None, "error": "Record not found"}}
            targets = [record]
        else:
            targets = [r for r in self._records.values() if r.enabled]

        for record in targets:
            if ip is None:
                record.last_error = "Could not determine current IP"
                results[record.id] = {"ok": False, "ip": None, "error": record.last_error}
                continue
            ok = await self._update_record(record, ip, ipv6)
            results[record.id] = {
                "ok": ok,
                "ip": ip,
                "error": record.last_error if not ok else None,
            }

        self._save()
        return results

    # ------------------------------------------------------------------
    # Internal loop
    # ------------------------------------------------------------------

    async def _check_loop(self) -> None:
        await self._do_all_updates()
        while True:
            await asyncio.sleep(self._config.check_interval_seconds)
            await self._do_all_updates()

    async def _do_all_updates(self) -> None:
        records = [r for r in self._records.values() if r.enabled]
        if not records:
            return

        ip = await self._get_current_ip()
        if ip is None:
            log.warning("ddns: could not determine current IPv4, skipping update cycle")
            return
        self._current_ipv4 = ip

        needs_ipv6 = any(r.ipv6 for r in records)
        ipv6: str | None = None
        if needs_ipv6:
            ipv6 = await self._get_current_ipv6()
            self._current_ipv6 = ipv6

        changed = False
        for record in records:
            ip_changed = record.last_ip != ip
            ipv6_changed = record.ipv6 and record.last_ipv6 != ipv6
            if not ip_changed and not ipv6_changed:
                log.debug("ddns: record %s (%s) — IP unchanged, skipping", record.id, record.name)
                continue
            ok = await self._update_record(record, ip, ipv6)
            if ok:
                log.info("ddns: record %s updated → %s", record.id, ip)
            else:
                log.warning("ddns: record %s update failed: %s", record.id, record.last_error)
            changed = True

        if changed:
            self._save()

    async def _update_record(self, record: DDNSRecord, current_ip: str, current_ipv6: str | None) -> bool:
        try:
            match record.provider:
                case "cloudflare":
                    ok = await self._update_cloudflare(record, current_ip, current_ipv6)
                case "namecheap":
                    ok = await self._update_namecheap(record, current_ip)
                case "duckdns":
                    ok = await self._update_duckdns(record, current_ip)
                case "noip":
                    ok = await self._update_noip(record, current_ip)
                case "dynu":
                    ok = await self._update_dynu(record, current_ip)
                case "gandi":
                    ok = await self._update_gandi(record, current_ip)
                case _:
                    record.last_error = f"Unknown provider: {record.provider}"
                    return False

            if ok:
                record.last_ip = current_ip
                if record.ipv6:
                    record.last_ipv6 = current_ipv6
                record.last_updated = time.time()
                record.last_error = None
            return ok
        except Exception as exc:
            record.last_error = str(exc)
            log.exception("ddns: exception updating record %s", record.id)
            return False

    # ------------------------------------------------------------------
    # IP detection
    # ------------------------------------------------------------------

    async def _get_current_ip(self) -> str | None:
        try:
            import aiohttp
        except ImportError:
            log.error("ddns: aiohttp not available")
            return None

        async with aiohttp.ClientSession() as session:
            for url in self._config.ip_providers:
                try:
                    async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as resp:
                        if resp.status == 200:
                            text = (await resp.text()).strip()
                            if _IPV4_RE.match(text):
                                return text
                            log.debug("ddns: IP provider %s returned non-IPv4: %r", url, text)
                except Exception as exc:
                    log.debug("ddns: IP provider %s failed: %s", url, exc)

        log.warning("ddns: all IPv4 providers failed")
        return None

    async def _get_current_ipv6(self) -> str | None:
        try:
            import aiohttp
        except ImportError:
            return None

        _ipv6_re = re.compile(r"^[0-9a-fA-F:]+$")

        async with aiohttp.ClientSession() as session:
            for url in self._config.ipv6_providers:
                try:
                    async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as resp:
                        if resp.status == 200:
                            text = (await resp.text()).strip()
                            if ":" in text and _ipv6_re.match(text):
                                return text
                except Exception as exc:
                    log.debug("ddns: IPv6 provider %s failed: %s", url, exc)

        return None

    # ------------------------------------------------------------------
    # Provider implementations
    # ------------------------------------------------------------------

    async def _update_cloudflare(self, record: DDNSRecord, ip: str, ipv6: str | None) -> bool:
        import aiohttp

        zone_id = record.credentials["zone_id"]
        api_token = record.credentials["api_token"]
        headers = {
            "Authorization": f"Bearer {api_token}",
            "Content-Type": "application/json",
        }
        base = f"https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records"
        ok = True

        async with aiohttp.ClientSession(headers=headers) as session:
            for hostname in record.hostnames:
                types = ["A"] if not record.ipv6 or ipv6 is None else ["A", "AAAA"]
                for rtype in types:
                    addr = ip if rtype == "A" else ipv6
                    if addr is None:
                        continue

                    # Find existing record
                    async with session.get(
                        base, params={"type": rtype, "name": hostname}
                    ) as resp:
                        data = await resp.json()
                        if not data.get("success"):
                            record.last_error = f"Cloudflare list error: {data.get('errors')}"
                            ok = False
                            continue

                    existing = data["result"]
                    payload = {
                        "type": rtype,
                        "name": hostname,
                        "content": addr,
                        "ttl": 60,
                        "proxied": False,
                    }

                    if existing:
                        dns_id = existing[0]["id"]
                        async with session.put(f"{base}/{dns_id}", json=payload) as resp:
                            data = await resp.json()
                            if not data.get("success"):
                                record.last_error = f"Cloudflare update error: {data.get('errors')}"
                                ok = False
                    else:
                        async with session.post(base, json=payload) as resp:
                            data = await resp.json()
                            if not data.get("success"):
                                record.last_error = f"Cloudflare create error: {data.get('errors')}"
                                ok = False

        return ok

    async def _update_namecheap(self, record: DDNSRecord, ip: str) -> bool:
        import aiohttp

        password = record.credentials["password"]
        ok = True

        async with aiohttp.ClientSession() as session:
            for hostname in record.hostnames:
                # hostname expected as "host.domain.tld" — split on first dot
                parts = hostname.split(".", 1)
                if len(parts) != 2:
                    record.last_error = f"Namecheap: cannot parse hostname {hostname!r}"
                    return False
                host, domain = parts

                params = {
                    "host": host,
                    "domain": domain,
                    "password": password,
                    "ip": ip,
                }
                url = "https://dynamicdns.park-your-domain.com/update"
                async with session.get(url, params=params, timeout=aiohttp.ClientTimeout(total=15)) as resp:
                    text = await resp.text()
                    if "<ErrCount>0</ErrCount>" not in text:
                        record.last_error = f"Namecheap error for {hostname}: {text[:200]}"
                        ok = False

        return ok

    async def _update_duckdns(self, record: DDNSRecord, ip: str) -> bool:
        import aiohttp

        token = record.credentials["token"]

        # Strip .duckdns.org suffix; accept bare subdomain names too
        domains = []
        for h in record.hostnames:
            bare = h.removesuffix(".duckdns.org")
            domains.append(bare)

        params = {
            "domains": ",".join(domains),
            "token": token,
            "ip": ip,
        }
        async with aiohttp.ClientSession() as session:
            async with session.get(
                "https://www.duckdns.org/update",
                params=params,
                timeout=aiohttp.ClientTimeout(total=15),
            ) as resp:
                text = (await resp.text()).strip()

        if text.startswith("OK"):
            return True
        record.last_error = f"DuckDNS returned: {text}"
        return False

    async def _update_noip(self, record: DDNSRecord, ip: str) -> bool:
        import aiohttp

        username = record.credentials["username"]
        password = record.credentials["password"]
        ok = True

        async with aiohttp.ClientSession() as session:
            for hostname in record.hostnames:
                params = {"hostname": hostname, "myip": ip}
                auth = aiohttp.BasicAuth(username, password)
                headers = {"User-Agent": "OzmaController/1.0 matt@ozma.dev"}
                async with session.get(
                    "https://dynupdate.no-ip.com/nic/update",
                    params=params,
                    auth=auth,
                    headers=headers,
                    timeout=aiohttp.ClientTimeout(total=15),
                ) as resp:
                    text = (await resp.text()).strip()

                if text.startswith("good") or text.startswith("nochg"):
                    pass  # success
                else:
                    record.last_error = f"No-IP error for {hostname}: {text}"
                    ok = False

        return ok

    async def _update_dynu(self, record: DDNSRecord, ip: str) -> bool:
        import aiohttp

        api_key = record.credentials["api_key"]
        headers = {"API-Key": api_key, "Content-Type": "application/json"}
        ok = True

        async with aiohttp.ClientSession(headers=headers) as session:
            # Fetch all DNS entries to find IDs for our hostnames
            async with session.get(
                "https://api.dynu.com/v2/dns",
                timeout=aiohttp.ClientTimeout(total=15),
            ) as resp:
                data = await resp.json()

            dns_list = data.get("dns", [])
            id_map = {entry["name"]: entry["id"] for entry in dns_list}

            for hostname in record.hostnames:
                dns_id = id_map.get(hostname)
                if dns_id is None:
                    record.last_error = f"Dynu: hostname {hostname!r} not found in account"
                    ok = False
                    continue

                payload = {"name": hostname, "ipv4Address": ip}
                async with session.post(
                    f"https://api.dynu.com/v2/dns/{dns_id}",
                    json=payload,
                    timeout=aiohttp.ClientTimeout(total=15),
                ) as resp:
                    if resp.status not in (200, 201):
                        body = await resp.text()
                        record.last_error = f"Dynu update error for {hostname}: HTTP {resp.status} {body[:200]}"
                        ok = False

        return ok

    async def _update_gandi(self, record: DDNSRecord, ip: str) -> bool:
        import aiohttp

        api_key = record.credentials["api_key"]
        domain = record.credentials["domain"]
        record_name = record.credentials.get("record_name", "@")
        headers = {
            "Authorization": f"Apikey {api_key}",
            "Content-Type": "application/json",
        }
        payload = {"rrset_ttl": 300, "rrset_values": [ip]}
        url = f"https://api.gandi.net/v5/livedns/domains/{domain}/records/{record_name}/A"

        async with aiohttp.ClientSession(headers=headers) as session:
            async with session.put(
                url, json=payload, timeout=aiohttp.ClientTimeout(total=15)
            ) as resp:
                if resp.status in (200, 201, 204):
                    return True
                body = await resp.text()
                record.last_error = f"Gandi error: HTTP {resp.status} {body[:200]}"
                return False

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        state = {
            "config": self._config.to_dict(),
            "records": {rid: r.to_dict() for rid, r in self._records.items()},
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
            state = json.loads(self._state_path.read_text())
            self._config = DDNSConfig.from_dict(state.get("config", {}))
            self._records = {
                rid: DDNSRecord.from_dict(rdata)
                for rid, rdata in state.get("records", {}).items()
            }
            log.info("ddns: loaded %d record(s) from %s", len(self._records), self._state_path)
        except Exception as exc:
            log.error("ddns: failed to load state from %s: %s", self._state_path, exc)
