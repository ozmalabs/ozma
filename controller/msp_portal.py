# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MSP White-label Client Portal.

Clients see a branded, read-only view of their own health data.
The portal HTML is self-contained (inline CSS via Tailwind CDN + minimal JS)
and has no external runtime dependencies beyond the CDN script tag.

Authentication: clients authenticate with their own OIDC token (from their
controller's IdP). The portal endpoint validates an OIDC token in the
Authorization header or ?token= query parameter, then renders only that
client's data.

For now the portal endpoint is accessible by client_id — the MSP is responsible
for gating access. A production deployment would front this with OIDC.
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from msp_dashboard import MSPDashboardManager, MSPClientHealth, MSPClient

log = logging.getLogger("ozma.msp_portal")


@dataclass
class PortalConfig:
    enabled: bool = False
    msp_name: str = "IT Support"
    msp_logo_url: str = ""
    primary_colour: str = "#2563eb"
    support_email: str = ""
    support_phone: str = ""
    show_compliance: bool = True
    show_machines: bool = True
    show_alerts: bool = True
    contact_creates_ticket: bool = True  # "Contact IT" → ITSM ticket

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "msp_name": self.msp_name,
            "msp_logo_url": self.msp_logo_url,
            "primary_colour": self.primary_colour,
            "support_email": self.support_email,
            "support_phone": self.support_phone,
            "show_compliance": self.show_compliance,
            "show_machines": self.show_machines,
            "show_alerts": self.show_alerts,
            "contact_creates_ticket": self.contact_creates_ticket,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PortalConfig:
        return cls(
            enabled=d.get("enabled", False),
            msp_name=d.get("msp_name", "IT Support"),
            msp_logo_url=d.get("msp_logo_url", ""),
            primary_colour=d.get("primary_colour", "#2563eb"),
            support_email=d.get("support_email", ""),
            support_phone=d.get("support_phone", ""),
            show_compliance=d.get("show_compliance", True),
            show_machines=d.get("show_machines", True),
            show_alerts=d.get("show_alerts", True),
            contact_creates_ticket=d.get("contact_creates_ticket", True),
        )


class MSPPortalManager:
    """White-label client portal. Renders per-client health as a self-contained HTML page."""

    def __init__(self, msp_mgr: MSPDashboardManager, config: PortalConfig):
        self._msp = msp_mgr
        self._config = config

    def get_portal_html(self, client: MSPClient, health: MSPClientHealth) -> str:
        """
        Returns a self-contained HTML page with Tailwind CDN for styling.
        Shows: client name, machine status, compliance score (if enabled),
        active alerts indicator, and a "Contact IT" button.
        Branding injected from PortalConfig.
        """
        cfg = self._config
        colour = cfg.primary_colour

        # Health badge colour
        health_colours = {
            "green": ("bg-green-100", "text-green-800", "All systems operational"),
            "amber": ("bg-yellow-100", "text-yellow-800", "Some attention needed"),
            "red": ("bg-red-100", "text-red-800", "Action required"),
        }
        h_bg, h_text, h_label = health_colours.get(
            health.health, ("bg-gray-100", "text-gray-800", "Unknown")
        )

        # Machine status
        machines_html = ""
        if cfg.show_machines:
            pct = (health.machines_online / health.machines_total * 100
                   if health.machines_total > 0 else 0)
            machines_html = f"""
        <div class="bg-white rounded-xl shadow p-6">
          <h2 class="text-lg font-semibold text-gray-700 mb-2">Machines</h2>
          <div class="flex items-end gap-2">
            <span class="text-4xl font-bold text-gray-900">{health.machines_online}</span>
            <span class="text-gray-500 mb-1">/ {health.machines_total} online</span>
          </div>
          <div class="mt-3 w-full bg-gray-200 rounded-full h-2">
            <div class="h-2 rounded-full" style="width:{pct:.0f}%; background-color:{colour};"></div>
          </div>
        </div>"""

        # Compliance section
        compliance_html = ""
        if cfg.show_compliance:
            e8_pct = int(health.e8_score * 100)
            iso_pct = int(health.iso27001_score * 100)
            overall_pct = int(health.compliance_score * 100)
            compliance_html = f"""
        <div class="bg-white rounded-xl shadow p-6">
          <h2 class="text-lg font-semibold text-gray-700 mb-4">Compliance</h2>
          <div class="space-y-3">
            <div>
              <div class="flex justify-between text-sm text-gray-600 mb-1">
                <span>Overall</span><span>{overall_pct}%</span>
              </div>
              <div class="w-full bg-gray-200 rounded-full h-2">
                <div class="h-2 rounded-full" style="width:{overall_pct}%; background-color:{colour};"></div>
              </div>
            </div>
            <div>
              <div class="flex justify-between text-sm text-gray-600 mb-1">
                <span>Essential Eight</span><span>{e8_pct}%</span>
              </div>
              <div class="w-full bg-gray-200 rounded-full h-2">
                <div class="h-2 rounded-full" style="width:{e8_pct}%; background-color:{colour};"></div>
              </div>
            </div>
            <div>
              <div class="flex justify-between text-sm text-gray-600 mb-1">
                <span>ISO 27001</span><span>{iso_pct}%</span>
              </div>
              <div class="w-full bg-gray-200 rounded-full h-2">
                <div class="h-2 rounded-full" style="width:{iso_pct}%; background-color:{colour};"></div>
              </div>
            </div>
          </div>
        </div>"""

        # Alerts section
        alerts_html = ""
        if cfg.show_alerts and health.critical_alerts > 0:
            alerts_html = f"""
        <div class="bg-red-50 border border-red-200 rounded-xl p-6">
          <h2 class="text-lg font-semibold text-red-700 mb-1">Active Alerts</h2>
          <p class="text-red-600">{health.critical_alerts} critical alert(s) require attention.</p>
          <p class="text-sm text-red-500 mt-1">Contact your IT support team for assistance.</p>
        </div>"""
        elif cfg.show_alerts:
            alerts_html = """
        <div class="bg-green-50 border border-green-200 rounded-xl p-6">
          <h2 class="text-lg font-semibold text-green-700 mb-1">Alerts</h2>
          <p class="text-green-600">No active alerts. All clear.</p>
        </div>"""

        # Pending items
        pending_html = ""
        if health.pending_approvals > 0:
            pending_html = f"""
        <div class="bg-yellow-50 border border-yellow-200 rounded-xl p-4">
          <p class="text-yellow-800 text-sm">
            <strong>{health.pending_approvals}</strong> action(s) pending IT approval.
          </p>
        </div>"""

        # Contact IT button
        contact_html = ""
        if cfg.support_email or cfg.support_phone:
            contact_parts = []
            if cfg.support_email:
                contact_parts.append(
                    f'<a href="mailto:{cfg.support_email}" '
                    f'class="inline-block px-6 py-3 rounded-lg text-white font-medium '
                    f'hover:opacity-90 transition" style="background-color:{colour};">'
                    f'Email Support</a>'
                )
            if cfg.support_phone:
                contact_parts.append(
                    f'<a href="tel:{cfg.support_phone}" '
                    f'class="inline-block px-6 py-3 rounded-lg border-2 font-medium '
                    f'hover:opacity-80 transition" '
                    f'style="border-color:{colour}; color:{colour};">'
                    f'Call {cfg.support_phone}</a>'
                )
            contact_html = f"""
        <div class="bg-white rounded-xl shadow p-6">
          <h2 class="text-lg font-semibold text-gray-700 mb-3">Need Help?</h2>
          <div class="flex flex-wrap gap-3">
            {"".join(contact_parts)}
          </div>
        </div>"""

        logo_html = ""
        if cfg.msp_logo_url:
            logo_html = f'<img src="{cfg.msp_logo_url}" alt="{cfg.msp_name}" class="h-8 mr-3">'

        return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{client.name} — {cfg.msp_name}</title>
  <script src="https://cdn.tailwindcss.com"></script>
</head>
<body class="bg-gray-50 min-h-screen">
  <!-- Header -->
  <header class="bg-white shadow-sm">
    <div class="max-w-4xl mx-auto px-4 py-4 flex items-center">
      {logo_html}
      <div>
        <span class="font-bold text-gray-900 text-lg">{cfg.msp_name}</span>
        <span class="text-gray-400 mx-2">|</span>
        <span class="text-gray-600">{client.name}</span>
      </div>
    </div>
  </header>

  <main class="max-w-4xl mx-auto px-4 py-8 space-y-6">
    <!-- Status banner -->
    <div class="rounded-xl px-6 py-4 flex items-center gap-3 {h_bg}">
      <span class="text-2xl">{'✓' if health.health == 'green' else '⚠' if health.health == 'amber' else '✗'}</span>
      <div>
        <p class="font-semibold {h_text}">{h_label}</p>
        <p class="text-sm {h_text} opacity-75">IT environment status for {client.name}</p>
      </div>
    </div>
{machines_html}
{compliance_html}
{alerts_html}
{pending_html}
{contact_html}

    <p class="text-center text-xs text-gray-400 pb-4">
      Managed by {cfg.msp_name} · Powered by Ozma
    </p>
  </main>
</body>
</html>"""

    def get_portal_config(self) -> PortalConfig:
        return self._config

    async def update_portal_config(self, **kwargs) -> PortalConfig:
        allowed = (
            "enabled", "msp_name", "msp_logo_url", "primary_colour",
            "support_email", "support_phone", "show_compliance",
            "show_machines", "show_alerts", "contact_creates_ticket",
        )
        for k, v in kwargs.items():
            if k in allowed:
                setattr(self._config, k, v)
        return self._config
