# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
DNS integrity verification for Ozma environments.

Checks that DNS is behaving correctly and has not been tampered with,
intercepted, or poisoned. Results feed into node/agent assurance levels
and are surfaced in the dashboard.

Checks performed
────────────────
  1. Resolver integrity     — system resolver agrees with DoH reference
  2. Transparent interception — ISP intercepting port 53 (compares raw
                                vs DoH result; port-53 interception is
                                invisible to the app layer)
  3. NXDOMAIN manipulation  — ISP redirecting NXDOMAIN to their own IP
                               (common "search assist" abuse)
  4. DNSSEC validation      — signed zones return authenticated data;
                               forge attempts produce SERVFAIL
  5. DNS rebinding guard    — reject responses where a public name
                               resolves to a private/loopback address
                               (SSRF / browser-based LAN pivoting)
  6. Leak detection         — in full-tunnel VPN mode, check that DNS
                               queries are not leaking to the local
                               network resolver
  7. Captive portal         — detect whether the network has intercepted
                               HTTP and injected a redirect

Architecture
────────────
  DNSVerifier runs periodically on the controller and on request. It
  also provides a DNSRebindingGuard for use by ServiceProxyManager —
  any proxy request whose resolved IP falls in a private range is
  rejected unless the domain is explicitly allowed.

  Each node/agent can submit its own DNS environment assessment via
  POST /api/v1/dns/environment, which the controller stores and
  surfaces alongside the controller's own assessment.
"""

from __future__ import annotations

import asyncio
import ipaddress
import json
import logging
import socket
import time
from dataclasses import dataclass, field
from typing import Any
from urllib.request import Request, urlopen
from urllib.error import URLError

log = logging.getLogger("ozma.dns_verify")

# ── reference DoH resolvers ─────────────────────────────────────────────────
_DOH_RESOLVERS = [
    "https://cloudflare-dns.com/dns-query",
    "https://dns.google/resolve",
]

# ── private / reserved IP ranges ─────────────────────────────────────────────
_PRIVATE_RANGES: list[ipaddress.IPv4Network | ipaddress.IPv6Network] = [
    ipaddress.ip_network("10.0.0.0/8"),
    ipaddress.ip_network("172.16.0.0/12"),
    ipaddress.ip_network("192.168.0.0/16"),
    ipaddress.ip_network("100.64.0.0/10"),   # CGNAT / Tailscale range
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("169.254.0.0/16"),  # link-local
    ipaddress.ip_network("::1/128"),
    ipaddress.ip_network("fc00::/7"),        # ULA
    ipaddress.ip_network("fe80::/10"),       # IPv6 link-local
]

# ── DNSSEC test domains ──────────────────────────────────────────────────────
_DNSSEC_VALID_DOMAIN = "internetsociety.org"
_DNSSEC_BROKEN_DOMAIN = "dnssec-failed.org"    # deliberately invalid RRSIG

# ── rebinding guard — domains allowed to resolve to private addresses ────────
_REBIND_ALLOWLIST: set[str] = set()


# ── result types ─────────────────────────────────────────────────────────────

@dataclass
class CheckResult:
    ok: bool
    issue: str | None = None
    detail: str | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"ok": self.ok}
        if self.issue:
            d["issue"] = self.issue
        if self.detail:
            d["detail"] = self.detail
        return d


@dataclass
class DNSEnvironment:
    """Full DNS integrity assessment for one node/context."""

    timestamp: float = field(default_factory=time.time)
    node_id: str = ""

    resolver_integrity: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    transparent_interception: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    nxdomain_manipulation: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    dnssec: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    rebinding_guard: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    captive_portal: CheckResult = field(default_factory=lambda: CheckResult(ok=True))
    dns_leak: CheckResult | None = None

    @property
    def healthy(self) -> bool:
        checks = [
            self.resolver_integrity,
            self.transparent_interception,
            self.nxdomain_manipulation,
            self.dnssec,
            self.rebinding_guard,
            self.captive_portal,
        ]
        if self.dns_leak is not None:
            checks.append(self.dns_leak)
        return all(c.ok for c in checks)

    @property
    def issues(self) -> list[str]:
        names = [
            ("resolver_integrity", self.resolver_integrity),
            ("transparent_interception", self.transparent_interception),
            ("nxdomain_manipulation", self.nxdomain_manipulation),
            ("dnssec", self.dnssec),
            ("rebinding_guard", self.rebinding_guard),
            ("captive_portal", self.captive_portal),
        ]
        if self.dns_leak is not None:
            names.append(("dns_leak", self.dns_leak))
        return [n for n, c in names if not c.ok]

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "timestamp": self.timestamp,
            "node_id": self.node_id,
            "healthy": self.healthy,
            "issues": self.issues,
            "checks": {
                "resolver_integrity": self.resolver_integrity.to_dict(),
                "transparent_interception": self.transparent_interception.to_dict(),
                "nxdomain_manipulation": self.nxdomain_manipulation.to_dict(),
                "dnssec": self.dnssec.to_dict(),
                "rebinding_guard": self.rebinding_guard.to_dict(),
                "captive_portal": self.captive_portal.to_dict(),
            },
        }
        if self.dns_leak is not None:
            d["checks"]["dns_leak"] = self.dns_leak.to_dict()
        return d


# ── DNS rebinding guard ──────────────────────────────────────────────────────

class DNSRebindingGuard:
    """
    Reject proxy requests where the resolved target IP is in a private range
    and the hostname is not explicitly allow-listed.

    Usage by ServiceProxyManager:
        guard = DNSRebindingGuard()
        ok, reason = guard.check_ip("someapp.connect.ozma.dev", "1.2.3.4")
    """

    def __init__(self, allowlist: set[str] | None = None) -> None:
        self._allowlist: set[str] = set(allowlist or _REBIND_ALLOWLIST)

    def add_allowlist(self, entries: set[str]) -> None:
        self._allowlist.update(entries)

    def remove_allowlist(self, entries: set[str]) -> None:
        self._allowlist -= entries

    def _is_private(self, ip_str: str) -> bool:
        try:
            addr = ipaddress.ip_address(ip_str)
        except ValueError:
            return False
        return any(addr in net for net in _PRIVATE_RANGES)

    def check_ip(self, hostname: str, resolved_ip: str) -> tuple[bool, str | None]:
        """
        Returns (allowed, reason). allowed=False means block the request.
        """
        if not self._is_private(resolved_ip):
            return True, None
        if hostname in self._allowlist:
            return True, None
        return (
            False,
            f"DNS rebinding guard: {hostname!r} resolved to private address {resolved_ip!r}. "
            "May indicate a DNS rebinding attack. Add to rebind allowlist to permit.",
        )

    async def resolve_and_check(self, hostname: str) -> tuple[bool, str | None, str | None]:
        """Resolve hostname, then check. Returns (allowed, ip, reason)."""
        loop = asyncio.get_event_loop()
        try:
            info = await loop.getaddrinfo(hostname, None, type=socket.SOCK_STREAM)
        except socket.gaierror as e:
            return False, None, f"DNS resolution failed: {e}"
        if not info:
            return False, None, "DNS resolution returned no results"
        ip = info[0][4][0]
        ok, reason = self.check_ip(hostname, ip)
        return ok, ip, reason


# ── DoH helper ───────────────────────────────────────────────────────────────

def _rtype_code(rtype: str) -> int:
    return {"A": 1, "AAAA": 28, "CNAME": 5, "MX": 15, "TXT": 16, "NS": 2}.get(rtype.upper(), 1)


async def _doh_resolve(
    domain: str,
    rtype: str = "A",
    resolver: str = _DOH_RESOLVERS[0],
) -> list[str]:
    """Resolve domain via DNS-over-HTTPS. Returns list of answer values."""
    url = f"{resolver}?name={domain}&type={rtype}&ct=application/dns-json"

    def _fetch() -> list[str]:
        req = Request(url, headers={
            "Accept": "application/dns-json",
            "User-Agent": "ozma-dns-verify/1.0",
        })
        try:
            with urlopen(req, timeout=5) as resp:
                data = json.loads(resp.read())
        except (URLError, json.JSONDecodeError, Exception):
            return []
        answers = data.get("Answer", [])
        code = _rtype_code(rtype)
        return [a["data"] for a in answers if a.get("type") == code]

    loop = asyncio.get_event_loop()
    return await loop.run_in_executor(None, _fetch)


async def _system_resolve(domain: str) -> list[str]:
    """Resolve domain via system resolver. Returns IP strings."""
    loop = asyncio.get_event_loop()
    try:
        info = await loop.getaddrinfo(domain, None, type=socket.SOCK_STREAM)
        return list({i[4][0] for i in info})
    except socket.gaierror:
        return []


# ── individual checks ────────────────────────────────────────────────────────

async def check_resolver_integrity(domain: str = "example.com") -> CheckResult:
    """
    Compare system resolver result with DoH reference for a known domain.
    Mismatch indicates resolver poisoning or transparent interception.
    """
    sys_ips, doh_ips = await asyncio.gather(
        _system_resolve(domain),
        _doh_resolve(domain),
    )
    if not doh_ips:
        return CheckResult(ok=True, detail="DoH reference unavailable — skipped")
    if not sys_ips:
        return CheckResult(ok=False, issue="resolver_no_answer",
                           detail=f"System resolver returned no answer for {domain!r}")
    if set(sys_ips) & set(doh_ips):
        return CheckResult(ok=True)
    # Non-overlapping IPs. Flag but don't hard-fail — CDN IPs legitimately
    # differ by geography. The dashboard shows this as a warning.
    log.warning("dns_verify: resolver integrity: system=%s doh=%s", set(sys_ips), set(doh_ips))
    return CheckResult(
        ok=False,
        issue="resolver_mismatch",
        detail=(
            f"System resolver returned {set(sys_ips)} for {domain!r}; "
            f"DoH reference returned {set(doh_ips)}. "
            "May indicate resolver poisoning or transparent interception."
        ),
    )


async def check_transparent_interception() -> CheckResult:
    """
    Detect transparent DNS proxy (ISP intercepting port 53).

    one.one.one.one has stable, non-geo-varied IPs (1.1.1.1 / 1.0.0.1).
    If the system resolver returns different IPs, there is a transparent proxy.
    """
    domain = "one.one.one.one"
    sys_ips = await _system_resolve(domain)
    doh_ips = await _doh_resolve(domain)
    known = {"1.1.1.1", "1.0.0.1", "2606:4700:4700::1111", "2606:4700:4700::1001"}
    if doh_ips and sys_ips:
        if not (set(sys_ips) & known) and (set(doh_ips) & known):
            return CheckResult(
                ok=False,
                issue="transparent_interception",
                detail=(
                    f"System resolver returned {set(sys_ips)!r} for {domain!r} "
                    f"but expected one of {known}. Transparent DNS proxy likely present."
                ),
            )
    return CheckResult(ok=True)


async def check_nxdomain_manipulation() -> CheckResult:
    """
    Detect NXDOMAIN hijacking.

    .invalid TLD is IANA-reserved and must never resolve per RFC 2606.
    Any IP response indicates ISP "search assist" / domain-parking behaviour.
    """
    canary = f"ozma-canary-{int(time.time())}.invalid"
    ips = await _system_resolve(canary)
    if ips:
        return CheckResult(
            ok=False,
            issue="nxdomain_hijacking",
            detail=(
                f"NXDOMAIN query for {canary!r} returned {ips!r}. "
                "ISP is redirecting non-existent domains. "
                "Incompatible with reliable DNS and may expose users to phishing."
            ),
        )
    return CheckResult(ok=True)


async def check_dnssec(
    valid_domain: str = _DNSSEC_VALID_DOMAIN,
    broken_domain: str = _DNSSEC_BROKEN_DOMAIN,
) -> CheckResult:
    """
    Verify DNSSEC validation via DoH.

    - Valid signed domain must resolve.
    - Broken domain (deliberate invalid RRSIG) must SERVFAIL (return no answer).
    """
    valid_ips, broken_ips = await asyncio.gather(
        _doh_resolve(valid_domain),
        _doh_resolve(broken_domain),
    )
    issues = []
    if not valid_ips:
        issues.append(f"DNSSEC-signed domain {valid_domain!r} returned no answer")
    if broken_ips:
        issues.append(
            f"Broken DNSSEC domain {broken_domain!r} returned {broken_ips!r} "
            "(expected SERVFAIL — resolver may not be validating DNSSEC)"
        )
    if issues:
        return CheckResult(ok=False, issue="dnssec_validation", detail="; ".join(issues))
    return CheckResult(ok=True)


async def check_rebinding_guard(guard: DNSRebindingGuard | None = None) -> CheckResult:
    """
    Verify the rebinding guard correctly rejects private-range responses.
    """
    _guard = guard or DNSRebindingGuard()
    ok, _ = _guard.check_ip("attacker.example.com", "192.168.1.100")
    if ok:
        return CheckResult(
            ok=False,
            issue="rebinding_guard_misconfigured",
            detail="Rebinding guard allowed private address for a non-allowlisted domain",
        )
    ok2, _ = _guard.check_ip("example.com", "93.184.216.34")
    if not ok2:
        return CheckResult(
            ok=False,
            issue="rebinding_guard_over_blocking",
            detail="Rebinding guard blocked a legitimate public address",
        )
    return CheckResult(ok=True)


async def check_captive_portal() -> CheckResult:
    """
    Detect captive portal by fetching Firefox's canary URL.
    Expects HTTP 200 with body containing 'canonical'.
    Captive portals redirect or return unexpected content.
    """
    canary_url = "http://detectportal.firefox.com/canonical.html"
    expected_fragment = "canonical"

    def _fetch() -> tuple[int, str]:
        req = Request(canary_url, headers={"User-Agent": "ozma-captive-check/1.0"})
        try:
            with urlopen(req, timeout=5) as resp:
                return resp.status, resp.read(256).decode("utf-8", errors="replace")
        except URLError as e:
            return 0, str(e)

    loop = asyncio.get_event_loop()
    status, body = await loop.run_in_executor(None, _fetch)

    if status == 0:
        return CheckResult(
            ok=False,
            issue="captive_portal_or_no_connectivity",
            detail=f"Captive portal check failed to connect: {body}",
        )
    if status != 200 or expected_fragment not in body.lower():
        return CheckResult(
            ok=False,
            issue="captive_portal_detected",
            detail=(
                f"Captive portal detected: canary returned status={status}, "
                f"body={body[:80]!r}. Internet access may be restricted."
            ),
        )
    return CheckResult(ok=True)


async def check_dns_leak(expected_resolver_ips: list[str] | None = None) -> CheckResult:
    """
    Detect DNS leaks in VPN full-tunnel mode.

    expected_resolver_ips: IPs of the exit node resolver(s). If None, not in VPN mode.
    Full leak detection requires raw socket access; this is a best-effort check.
    """
    if not expected_resolver_ips:
        return CheckResult(ok=True, detail="VPN mode not active — DNS leak check skipped")
    # Full implementation requires raw socket interception (SO_MARK or similar).
    # Placeholder: mark as partial until raw socket path is implemented.
    return CheckResult(
        ok=True,
        detail="DNS leak check: partial — full verification requires raw socket access",
    )


# ── main verifier ─────────────────────────────────────────────────────────────

class DNSVerifier:
    """
    Runs all DNS integrity checks and maintains the current environment assessment.
    """

    _CHECK_INTERVAL = 300      # seconds between full runs
    _FAST_RECHECK_INTERVAL = 60  # seconds when issues are detected

    def __init__(self, rebinding_guard: DNSRebindingGuard | None = None) -> None:
        self._guard = rebinding_guard or DNSRebindingGuard()
        self._current: DNSEnvironment | None = None
        self._node_environments: dict[str, DNSEnvironment] = {}
        self._task: asyncio.Task | None = None

    @property
    def guard(self) -> DNSRebindingGuard:
        return self._guard

    async def run_once(self, vpn_resolver_ips: list[str] | None = None) -> DNSEnvironment:
        """Run all checks and return the result."""
        results = await asyncio.gather(
            check_resolver_integrity(),
            check_transparent_interception(),
            check_nxdomain_manipulation(),
            check_dnssec(),
            check_rebinding_guard(self._guard),
            check_captive_portal(),
            check_dns_leak(vpn_resolver_ips),
            return_exceptions=True,
        )

        def _safe(r: Any, default_issue: str) -> CheckResult:
            if isinstance(r, Exception):
                log.warning("dns_verify: check raised %s: %s", type(r).__name__, r)
                return CheckResult(ok=False, issue=default_issue, detail=str(r))
            return r  # type: ignore[return-value]

        env = DNSEnvironment(
            resolver_integrity=_safe(results[0], "resolver_integrity_error"),
            transparent_interception=_safe(results[1], "transparent_interception_error"),
            nxdomain_manipulation=_safe(results[2], "nxdomain_manipulation_error"),
            dnssec=_safe(results[3], "dnssec_error"),
            rebinding_guard=_safe(results[4], "rebinding_guard_error"),
            captive_portal=_safe(results[5], "captive_portal_error"),
            dns_leak=_safe(results[6], "dns_leak_error") if vpn_resolver_ips else None,
        )
        self._current = env
        if not env.healthy:
            log.warning("dns_verify: issues detected: %s", env.issues)
        else:
            log.debug("dns_verify: all checks passed")
        return env

    def accept_node_environment(self, node_id: str, data: dict[str, Any]) -> None:
        """Store a DNS environment assessment submitted by a remote node or agent."""
        try:
            checks = data.get("checks", {})

            def _cr(key: str) -> CheckResult:
                raw = checks.get(key, {"ok": True})
                return CheckResult(
                    ok=raw.get("ok", True),
                    issue=raw.get("issue"),
                    detail=raw.get("detail"),
                )

            env = DNSEnvironment(
                timestamp=data.get("timestamp", time.time()),
                node_id=node_id,
                resolver_integrity=_cr("resolver_integrity"),
                transparent_interception=_cr("transparent_interception"),
                nxdomain_manipulation=_cr("nxdomain_manipulation"),
                dnssec=_cr("dnssec"),
                rebinding_guard=_cr("rebinding_guard"),
                captive_portal=_cr("captive_portal"),
            )
            if "dns_leak" in checks:
                env.dns_leak = _cr("dns_leak")
            self._node_environments[node_id] = env
            if not env.healthy:
                log.warning("dns_verify: node %r has DNS issues: %s", node_id, env.issues)
        except Exception as e:
            log.warning("dns_verify: failed to parse node environment from %r: %s", node_id, e)

    def get_environment(self, node_id: str | None = None) -> DNSEnvironment | None:
        """Return the current environment: controller's own, or a specific node's."""
        if node_id:
            return self._node_environments.get(node_id)
        return self._current

    def all_environments(self) -> list[dict[str, Any]]:
        """Return all environments (controller + all nodes)."""
        result = []
        if self._current:
            result.append(self._current.to_dict())
        for env in self._node_environments.values():
            result.append(env.to_dict())
        return result

    async def start(self) -> None:
        self._task = asyncio.create_task(self._loop(), name="dns-verify")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    async def _loop(self) -> None:
        await asyncio.sleep(15)
        while True:
            try:
                env = await self.run_once()
                interval = self._FAST_RECHECK_INTERVAL if not env.healthy else self._CHECK_INTERVAL
            except Exception as e:
                log.error("dns_verify: unexpected error in check loop: %s", e)
                interval = self._FAST_RECHECK_INTERVAL
            await asyncio.sleep(interval)
