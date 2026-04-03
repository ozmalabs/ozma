# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
ITSM — IT Service Management with intelligent agent triage and on-call routing.

Ticket lifecycle:
  Created → L1 triage (Haiku-fast, auto-remediation)
          → resolved                               ✓
          → L1 max attempts / timeout →
  L2 triage (Opus-expert, deep diagnosis)
          → resolved                               ✓
          → L2 max attempts / timeout →
  Human queue — notified via on-call schedule
          → acknowledged + resolved                ✓

On-call notification rules (PagerDuty-style):
  - Notify if: within working hours OR active on-call window OR opted-in to interruptions
  - Critical always interrupts (configurable per user)
  - Escalate to next tier if no acknowledgment within timeout
  - External ITSM webhook for Jira/Freshservice/Zendesk integration

Agent triage is event-driven: the manager fires structured events that the
MCP-connected AI agent receives and acts on. The AI uses ozma_control to
diagnose/fix the issue, then calls back via the ITSM API endpoints.
"""

from __future__ import annotations

import asyncio
import datetime
import json
import logging
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Awaitable

log = logging.getLogger("ozma.itsm")

# ── Agent tiers ───────────────────────────────────────────────────────────────

AGENT_L1 = "l1"        # Fast/cheap — auto-remediation for common issues
AGENT_L2 = "l2"        # Expert — deep diagnosis, complex remediation
AGENT_HUMAN = "human"  # Human intervention required

# ── SLA defaults (seconds) ────────────────────────────────────────────────────

SLA_RESPONSE: dict[str, int] = {
    "critical": 3_600,          # 1 h
    "high":     4 * 3_600,      # 4 h
    "medium":   24 * 3_600,     # 1 d
    "low":      3 * 24 * 3_600, # 3 d
}

SLA_RESOLUTION: dict[str, int] = {
    "critical": 4 * 3_600,
    "high":     24 * 3_600,
    "medium":   3 * 24 * 3_600,
    "low":      7 * 24 * 3_600,
}

_PRIORITIES = ("critical", "high", "medium", "low")
_TICKET_STATUSES = (
    "open", "l1_triage", "l2_triage", "pending_human",
    "acknowledged", "resolved", "closed",
)


# ── Ticket ────────────────────────────────────────────────────────────────────

@dataclass
class TicketAuditEntry:
    timestamp: float
    actor: str      # "system" | "agent_l1" | "agent_l2" | user_id
    action: str
    note: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {"timestamp": self.timestamp, "actor": self.actor,
                "action": self.action, "note": self.note}

    @classmethod
    def from_dict(cls, d: dict) -> "TicketAuditEntry":
        return cls(timestamp=d["timestamp"], actor=d["actor"],
                   action=d["action"], note=d.get("note", ""))


@dataclass
class Ticket:
    id: str                             # TKT-YYYY-NNNN
    created_at: float
    source: str                         # "helpdesk" | "agent" | "user" | "alert" | "ocr_trigger"
    category: str                       # "access_request" | "hardware" | "software" | "security" | "change" | "incident"
    priority: str                       # "low" | "medium" | "high" | "critical"
    subject: str
    description: str
    requester_user_id: str
    assignee_user_id: str | None = None
    node_id: str | None = None          # affected machine
    status: str = "open"
    agent_tier: str = AGENT_L1         # current/last tier attempted
    l1_attempts: int = 0
    l2_attempts: int = 0
    sla_response_deadline: float | None = None
    sla_resolution_deadline: float | None = None
    responded_at: float | None = None
    resolved_at: float | None = None
    resolution: str = ""
    external_ref: str = ""              # Jira/Freshservice/Zendesk ticket ID
    acknowledged_by: str | None = None
    acknowledged_at: float | None = None
    escalation_tier_index: int = 0      # which EscalationPolicy tier we're on
    audit: list[TicketAuditEntry] = field(default_factory=list)

    # ── Audit helpers ──────────────────────────────────────────────────

    def add_audit(self, actor: str, action: str, note: str = "") -> None:
        self.audit.append(TicketAuditEntry(
            timestamp=time.time(), actor=actor, action=action, note=note,
        ))

    # ── SLA helpers ────────────────────────────────────────────────────

    @property
    def sla_response_breached(self) -> bool:
        if not self.sla_response_deadline or self.responded_at:
            return False
        return time.time() > self.sla_response_deadline

    @property
    def sla_resolution_breached(self) -> bool:
        if not self.sla_resolution_deadline or self.status in ("resolved", "closed"):
            return False
        return time.time() > self.sla_resolution_deadline

    def sla_response_fraction(self) -> float:
        """0.0–1.0 fraction of SLA response deadline consumed. >1.0 = breached."""
        if not self.sla_response_deadline:
            return 0.0
        total = self.sla_response_deadline - self.created_at
        return (time.time() - self.created_at) / total if total > 0 else 1.0

    def sla_resolution_fraction(self) -> float:
        if not self.sla_resolution_deadline:
            return 0.0
        total = self.sla_resolution_deadline - self.created_at
        return (time.time() - self.created_at) / total if total > 0 else 1.0

    # ── Serialisation ──────────────────────────────────────────────────

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "created_at": self.created_at,
            "source": self.source,
            "category": self.category,
            "priority": self.priority,
            "subject": self.subject,
            "description": self.description,
            "requester_user_id": self.requester_user_id,
            "assignee_user_id": self.assignee_user_id,
            "node_id": self.node_id,
            "status": self.status,
            "agent_tier": self.agent_tier,
            "l1_attempts": self.l1_attempts,
            "l2_attempts": self.l2_attempts,
            "sla_response_deadline": self.sla_response_deadline,
            "sla_resolution_deadline": self.sla_resolution_deadline,
            "responded_at": self.responded_at,
            "resolved_at": self.resolved_at,
            "resolution": self.resolution,
            "external_ref": self.external_ref,
            "acknowledged_by": self.acknowledged_by,
            "acknowledged_at": self.acknowledged_at,
            "escalation_tier_index": self.escalation_tier_index,
            "audit": [a.to_dict() for a in self.audit],
            # Computed fields (read-only, not stored)
            "sla_response_breached": self.sla_response_breached,
            "sla_resolution_breached": self.sla_resolution_breached,
            "sla_response_fraction": self.sla_response_fraction(),
            "sla_resolution_fraction": self.sla_resolution_fraction(),
        }

    @classmethod
    def from_dict(cls, d: dict) -> "Ticket":
        return cls(
            id=d["id"],
            created_at=d["created_at"],
            source=d.get("source", "unknown"),
            category=d.get("category", "incident"),
            priority=d.get("priority", "medium"),
            subject=d.get("subject", ""),
            description=d.get("description", ""),
            requester_user_id=d.get("requester_user_id", "system"),
            assignee_user_id=d.get("assignee_user_id"),
            node_id=d.get("node_id"),
            status=d.get("status", "open"),
            agent_tier=d.get("agent_tier", AGENT_L1),
            l1_attempts=d.get("l1_attempts", 0),
            l2_attempts=d.get("l2_attempts", 0),
            sla_response_deadline=d.get("sla_response_deadline"),
            sla_resolution_deadline=d.get("sla_resolution_deadline"),
            responded_at=d.get("responded_at"),
            resolved_at=d.get("resolved_at"),
            resolution=d.get("resolution", ""),
            external_ref=d.get("external_ref", ""),
            acknowledged_by=d.get("acknowledged_by"),
            acknowledged_at=d.get("acknowledged_at"),
            escalation_tier_index=d.get("escalation_tier_index", 0),
            audit=[TicketAuditEntry.from_dict(a) for a in d.get("audit", [])],
        )


# ── On-call schedule ──────────────────────────────────────────────────────────

@dataclass
class WorkingHours:
    """Working hours for one day of the week."""
    day: int    # 0 = Monday … 6 = Sunday
    start: int  # hour 0–23 (inclusive)
    end: int    # hour 0–23 (exclusive)

    def to_dict(self) -> dict[str, Any]:
        return {"day": self.day, "start": self.start, "end": self.end}

    @classmethod
    def from_dict(cls, d: dict) -> "WorkingHours":
        return cls(day=d["day"], start=d["start"], end=d["end"])


@dataclass
class OnCallWindow:
    """Explicit on-call override for a calendar range (overrides working hours)."""
    start_ts: float
    end_ts: float
    note: str = ""

    def active(self) -> bool:
        return self.start_ts <= time.time() <= self.end_ts

    def to_dict(self) -> dict[str, Any]:
        return {"start_ts": self.start_ts, "end_ts": self.end_ts, "note": self.note}

    @classmethod
    def from_dict(cls, d: dict) -> "OnCallWindow":
        return cls(start_ts=d["start_ts"], end_ts=d["end_ts"], note=d.get("note", ""))


@dataclass
class OnCallUser:
    """A user who can receive ITSM notifications, with their availability rules."""
    user_id: str
    # Destination IDs from NotificationManager (slack_webhook, discord_webhook, email, etc.)
    channels: list[str] = field(default_factory=list)
    working_hours: list[WorkingHours] = field(default_factory=list)
    oncall_windows: list[OnCallWindow] = field(default_factory=list)
    # Interrupt preferences (for out-of-hours notifications)
    interrupt_critical: bool = True   # always wake for P1, even at 3am
    interrupt_high: bool = False      # wake for P2 outside hours
    interrupt_any: bool = False       # opt-in: always notify regardless of priority

    def is_available(self, priority: str = "medium") -> bool:
        """Return True if this user should be notified right now."""
        if self.interrupt_any:
            return True
        if any(w.active() for w in self.oncall_windows):
            return True
        if self._in_working_hours():
            return True
        # Out of hours — check per-priority opt-ins
        if priority == "critical" and self.interrupt_critical:
            return True
        if priority == "high" and self.interrupt_high:
            return True
        return False

    def _in_working_hours(self) -> bool:
        now = datetime.datetime.now()
        for wh in self.working_hours:
            if now.weekday() == wh.day and wh.start <= now.hour < wh.end:
                return True
        return False

    def next_available_at(self) -> float | None:
        """Return timestamp of next working-hours start, or None if no schedule."""
        if not self.working_hours:
            return None
        now = datetime.datetime.now()
        for day_offset in range(8):
            candidate = now + datetime.timedelta(days=day_offset)
            for wh in self.working_hours:
                if candidate.weekday() == wh.day:
                    candidate_start = candidate.replace(
                        hour=wh.start, minute=0, second=0, microsecond=0
                    )
                    if candidate_start > now:
                        return candidate_start.timestamp()
        return None

    def to_dict(self) -> dict[str, Any]:
        return {
            "user_id": self.user_id,
            "channels": self.channels,
            "working_hours": [w.to_dict() for w in self.working_hours],
            "oncall_windows": [w.to_dict() for w in self.oncall_windows],
            "interrupt_critical": self.interrupt_critical,
            "interrupt_high": self.interrupt_high,
            "interrupt_any": self.interrupt_any,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "OnCallUser":
        return cls(
            user_id=d["user_id"],
            channels=d.get("channels", []),
            working_hours=[WorkingHours.from_dict(w) for w in d.get("working_hours", [])],
            oncall_windows=[OnCallWindow.from_dict(w) for w in d.get("oncall_windows", [])],
            interrupt_critical=d.get("interrupt_critical", True),
            interrupt_high=d.get("interrupt_high", False),
            interrupt_any=d.get("interrupt_any", False),
        )


# ── Escalation policy ─────────────────────────────────────────────────────────

@dataclass
class EscalationTier:
    """One tier in an escalation policy — notifies user_ids, then waits for ack."""
    user_ids: list[str]
    ack_timeout_seconds: int = 900    # 15 min default; move to next tier if no ack
    channels_override: list[str] = field(default_factory=list)  # per-tier channel override

    def to_dict(self) -> dict[str, Any]:
        return {
            "user_ids": self.user_ids,
            "ack_timeout_seconds": self.ack_timeout_seconds,
            "channels_override": self.channels_override,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "EscalationTier":
        return cls(
            user_ids=d["user_ids"],
            ack_timeout_seconds=d.get("ack_timeout_seconds", 900),
            channels_override=d.get("channels_override", []),
        )


@dataclass
class EscalationPolicy:
    """Ordered escalation tiers. Tiers are tried in order until acknowledgment."""
    id: str
    name: str
    tiers: list[EscalationTier] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.id, "name": self.name,
                "tiers": [t.to_dict() for t in self.tiers]}

    @classmethod
    def from_dict(cls, d: dict) -> "EscalationPolicy":
        return cls(
            id=d["id"], name=d["name"],
            tiers=[EscalationTier.from_dict(t) for t in d.get("tiers", [])],
        )


# ── Agent model configuration ─────────────────────────────────────────────────

@dataclass
class AgentModelConfig:
    """
    LLM configuration for one agent tier.

    ``provider`` selects the inference backend; ``model`` is the provider-specific
    model identifier.  ``base_url`` overrides the default API endpoint — required
    for self-hosted (Ollama, LM Studio, vLLM) and OpenAI-compatible proxies.
    ``api_key_env`` is the *name* of an environment variable that holds the key
    (never store the key itself in config).  ``extra`` passes arbitrary provider
    parameters (temperature, max_tokens, system_prompt override, etc.).

    Provider examples:
      anthropic  model="claude-haiku-4-5-20251001"
      openai     model="gpt-4o-mini"
      ollama     model="llama3.2:3b"  base_url="http://localhost:11434"
      groq       model="llama-3.1-8b-instant"
      bedrock    model="us.anthropic.claude-haiku-4-5-20251001-v1:0"
      openai-compat  model="qwen2.5-coder-7b"  base_url="http://my-vllm:8000/v1"
    """
    provider: str = "anthropic"
    model: str = ""
    base_url: str = ""              # empty = use provider default
    api_key_env: str = ""           # env var name, not the key value
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "provider": self.provider,
            "model": self.model,
            "base_url": self.base_url,
            "api_key_env": self.api_key_env,
            "extra": self.extra,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "AgentModelConfig":
        return cls(
            provider=d.get("provider", "anthropic"),
            model=d.get("model", ""),
            base_url=d.get("base_url", ""),
            api_key_env=d.get("api_key_env", ""),
            extra=d.get("extra", {}),
        )


# Defaults — used when no config has been saved yet.
# Override via PATCH /api/v1/itsm/config or edit itsm_data/itsm_config.json.
_DEFAULT_L1_MODEL = AgentModelConfig(
    provider="anthropic",
    model="claude-haiku-4-5-20251001",
)
_DEFAULT_L2_MODEL = AgentModelConfig(
    provider="anthropic",
    model="claude-opus-4-6",
)


# ── Config ────────────────────────────────────────────────────────────────────

@dataclass
class ITSMConfig:
    oncall_users: dict[str, OnCallUser] = field(default_factory=dict)
    escalation_policies: dict[str, EscalationPolicy] = field(default_factory=dict)
    default_policy_id: str = ""
    # Agent triage limits
    l1_max_attempts: int = 2
    l2_max_attempts: int = 1
    l1_timeout_seconds: int = 300     # give L1 5 min before promoting to L2
    l2_timeout_seconds: int = 900     # give L2 15 min before sending to human
    # LLM model configuration per tier
    l1_model: AgentModelConfig = field(
        default_factory=lambda: AgentModelConfig(
            provider="anthropic", model="claude-haiku-4-5-20251001"
        )
    )
    l2_model: AgentModelConfig = field(
        default_factory=lambda: AgentModelConfig(
            provider="anthropic", model="claude-opus-4-6"
        )
    )
    # External ITSM (Jira/Freshservice/Zendesk webhook)
    external_webhook_url: str = ""
    external_webhook_headers: dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "oncall_users": {uid: u.to_dict() for uid, u in self.oncall_users.items()},
            "escalation_policies": {pid: p.to_dict() for pid, p in self.escalation_policies.items()},
            "default_policy_id": self.default_policy_id,
            "l1_max_attempts": self.l1_max_attempts,
            "l2_max_attempts": self.l2_max_attempts,
            "l1_timeout_seconds": self.l1_timeout_seconds,
            "l2_timeout_seconds": self.l2_timeout_seconds,
            "l1_model": self.l1_model.to_dict(),
            "l2_model": self.l2_model.to_dict(),
            "external_webhook_url": self.external_webhook_url,
            "external_webhook_headers": self.external_webhook_headers,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "ITSMConfig":
        return cls(
            oncall_users={uid: OnCallUser.from_dict(u)
                          for uid, u in d.get("oncall_users", {}).items()},
            escalation_policies={pid: EscalationPolicy.from_dict(p)
                                  for pid, p in d.get("escalation_policies", {}).items()},
            default_policy_id=d.get("default_policy_id", ""),
            l1_max_attempts=d.get("l1_max_attempts", 2),
            l2_max_attempts=d.get("l2_max_attempts", 1),
            l1_timeout_seconds=d.get("l1_timeout_seconds", 300),
            l2_timeout_seconds=d.get("l2_timeout_seconds", 900),
            l1_model=AgentModelConfig.from_dict(d["l1_model"]) if "l1_model" in d
                     else AgentModelConfig(provider="anthropic", model="claude-haiku-4-5-20251001"),
            l2_model=AgentModelConfig.from_dict(d["l2_model"]) if "l2_model" in d
                     else AgentModelConfig(provider="anthropic", model="claude-opus-4-6"),
            external_webhook_url=d.get("external_webhook_url", ""),
            external_webhook_headers=d.get("external_webhook_headers", {}),
        )


# ── Manager ───────────────────────────────────────────────────────────────────

_SLA_WARN_FRACTIONS = (0.5, 0.75, 1.0)  # fire warnings at 50%, 75%, 100% of SLA


class ITSMManager:
    """
    IT Service Management — ticket lifecycle, agent triage, on-call notification.

    Agent triage is event-driven: the manager fires ``itsm.ticket.triage`` events
    with the ticket payload and a ``model_hint`` field.  An MCP-connected AI agent
    picks up the event, runs ozma_control actions, then calls back via:

      POST /api/v1/itsm/tickets/{id}/resolve   → marks resolved
      POST /api/v1/itsm/tickets/{id}/escalate  → promotes to next tier

    If no agent responds within the tier timeout, the manager auto-escalates.
    """

    def __init__(
        self,
        data_path: Path,
        config: ITSMConfig | None = None,
        notifier: Any = None,
        event_queue: asyncio.Queue | None = None,
    ) -> None:
        self._path = data_path
        self._tickets_path = data_path / "tickets.json"
        self._config_path = data_path / "itsm_config.json"
        self._tickets: dict[str, Ticket] = {}
        self._config = config or ITSMConfig()
        self._notifier = notifier
        self._event_queue = event_queue
        self._year_counters: dict[int, int] = {}   # year → last ticket number
        self._tasks: list[asyncio.Task] = []
        # Per-ticket triage timeout tasks (ticket_id → Task)
        self._triage_timers: dict[str, asyncio.Task] = {}
        # Per-ticket escalation timeout tasks
        self._escalation_timers: dict[str, asyncio.Task] = {}
        self._load()

    # ── Persistence ───────────────────────────────────────────────────

    def _load(self) -> None:
        self._path.mkdir(parents=True, exist_ok=True)
        # Load config
        if self._config_path.exists():
            try:
                self._config = ITSMConfig.from_dict(
                    json.loads(self._config_path.read_text())
                )
            except Exception as e:
                log.warning("Failed to load ITSM config: %s", e)
        # Load tickets
        if self._tickets_path.exists():
            try:
                raw = json.loads(self._tickets_path.read_text())
                for d in raw.get("tickets", []):
                    t = Ticket.from_dict(d)
                    self._tickets[t.id] = t
                self._year_counters = {
                    int(k): v for k, v in raw.get("year_counters", {}).items()
                }
            except Exception as e:
                log.warning("Failed to load ITSM tickets: %s", e)
        log.info("ITSM loaded: %d tickets", len(self._tickets))

    def _save_tickets(self) -> None:
        try:
            data = {
                "tickets": [t.to_dict() for t in self._tickets.values()],
                "year_counters": self._year_counters,
            }
            self._tickets_path.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save ITSM tickets: %s", e)

    def save_config(self) -> None:
        try:
            self._config_path.write_text(
                json.dumps(self._config.to_dict(), indent=2)
            )
        except Exception as e:
            log.error("Failed to save ITSM config: %s", e)

    # ── Ticket ID generation ──────────────────────────────────────────

    def _next_ticket_id(self) -> str:
        year = datetime.datetime.now().year
        n = self._year_counters.get(year, 0) + 1
        self._year_counters[year] = n
        return f"TKT-{year}-{n:04d}"

    # ── Public API ────────────────────────────────────────────────────

    async def create_ticket(
        self,
        source: str,
        category: str,
        priority: str,
        subject: str,
        description: str,
        requester_user_id: str = "system",
        node_id: str | None = None,
        assignee_user_id: str | None = None,
    ) -> Ticket:
        """Create a ticket and kick off L1 triage."""
        now = time.time()
        ticket = Ticket(
            id=self._next_ticket_id(),
            created_at=now,
            source=source,
            category=category,
            priority=priority if priority in _PRIORITIES else "medium",
            subject=subject,
            description=description,
            requester_user_id=requester_user_id,
            assignee_user_id=assignee_user_id,
            node_id=node_id,
            sla_response_deadline=now + SLA_RESPONSE.get(priority, SLA_RESPONSE["medium"]),
            sla_resolution_deadline=now + SLA_RESOLUTION.get(priority, SLA_RESOLUTION["medium"]),
        )
        ticket.add_audit("system", "created", f"Source: {source}")
        self._tickets[ticket.id] = ticket
        self._save_tickets()

        await self._fire_event("itsm.ticket.created", ticket.to_dict())
        await self._push_to_external(ticket)
        await self._start_l1_triage(ticket)
        return ticket

    def get_ticket(self, ticket_id: str) -> Ticket | None:
        return self._tickets.get(ticket_id)

    def list_tickets(
        self,
        status: str | None = None,
        priority: str | None = None,
        limit: int = 100,
    ) -> list[Ticket]:
        tickets = list(self._tickets.values())
        if status:
            tickets = [t for t in tickets if t.status == status]
        if priority:
            tickets = [t for t in tickets if t.priority == priority]
        tickets.sort(key=lambda t: t.created_at, reverse=True)
        return tickets[:limit]

    async def resolve_ticket(
        self,
        ticket_id: str,
        actor: str,
        resolution: str,
    ) -> bool:
        """Mark a ticket resolved. Called by AI agent or human."""
        ticket = self._tickets.get(ticket_id)
        if not ticket or ticket.status in ("resolved", "closed"):
            return False
        now = time.time()
        tier = ticket.agent_tier
        ticket.status = "resolved"
        ticket.resolved_at = now
        ticket.resolution = resolution
        if not ticket.responded_at:
            ticket.responded_at = now
        ticket.add_audit(actor, "resolved",
                         f"Resolution by {tier} agent: {resolution[:200]}")
        self._cancel_timers(ticket_id)
        self._save_tickets()
        await self._fire_event("itsm.ticket.resolved", ticket.to_dict())
        log.info("Ticket %s resolved by %s (%s)", ticket_id, actor, tier)
        return True

    async def escalate_ticket(
        self,
        ticket_id: str,
        actor: str,
        notes: str = "",
    ) -> bool:
        """
        Escalate a ticket to the next tier.

        L1 → L2 → human.  Called by the AI agent when it cannot fix the issue,
        or by the auto-timeout logic when no agent responds in time.
        """
        ticket = self._tickets.get(ticket_id)
        if not ticket or ticket.status in ("resolved", "closed"):
            return False
        self._cancel_timers(ticket_id)
        if ticket.agent_tier == AGENT_L1:
            await self._start_l2_triage(ticket, notes)
        elif ticket.agent_tier == AGENT_L2:
            await self._escalate_to_human(ticket, notes)
        else:
            # Already at human; advance escalation tier
            await self._advance_escalation_tier(ticket)
        return True

    async def acknowledge_ticket(
        self,
        ticket_id: str,
        user_id: str,
    ) -> bool:
        """Human acknowledges they are handling the ticket."""
        ticket = self._tickets.get(ticket_id)
        if not ticket or ticket.status not in ("pending_human", "l1_triage", "l2_triage"):
            return False
        now = time.time()
        ticket.acknowledged_by = user_id
        ticket.acknowledged_at = now
        ticket.status = "acknowledged"
        if not ticket.responded_at:
            ticket.responded_at = now
        ticket.add_audit(user_id, "acknowledged")
        self._cancel_timers(ticket_id)
        self._save_tickets()
        await self._fire_event("itsm.ticket.acknowledged", ticket.to_dict())
        return True

    async def close_ticket(self, ticket_id: str, actor: str) -> bool:
        ticket = self._tickets.get(ticket_id)
        if not ticket:
            return False
        ticket.status = "closed"
        ticket.add_audit(actor, "closed")
        self._cancel_timers(ticket_id)
        self._save_tickets()
        await self._fire_event("itsm.ticket.closed", ticket.to_dict())
        return True

    async def comment_ticket(
        self,
        ticket_id: str,
        actor: str,
        note: str,
    ) -> bool:
        ticket = self._tickets.get(ticket_id)
        if not ticket:
            return False
        ticket.add_audit(actor, "comment", note)
        self._save_tickets()
        return True

    # ── Config management ─────────────────────────────────────────────

    def get_config(self) -> ITSMConfig:
        return self._config

    def set_config(self, config: ITSMConfig) -> None:
        self._config = config
        self.save_config()

    def upsert_oncall_user(self, user: OnCallUser) -> None:
        self._config.oncall_users[user.user_id] = user
        self.save_config()

    def remove_oncall_user(self, user_id: str) -> bool:
        if user_id not in self._config.oncall_users:
            return False
        del self._config.oncall_users[user_id]
        self.save_config()
        return True

    def upsert_escalation_policy(self, policy: EscalationPolicy) -> None:
        self._config.escalation_policies[policy.id] = policy
        self.save_config()

    def remove_escalation_policy(self, policy_id: str) -> bool:
        if policy_id not in self._config.escalation_policies:
            return False
        del self._config.escalation_policies[policy_id]
        if self._config.default_policy_id == policy_id:
            self._config.default_policy_id = ""
        self.save_config()
        return True

    # ── Agent triage ──────────────────────────────────────────────────

    async def _start_l1_triage(self, ticket: Ticket) -> None:
        ticket.status = "l1_triage"
        ticket.agent_tier = AGENT_L1
        ticket.l1_attempts += 1
        ticket.add_audit("system", "l1_triage_started",
                         f"Attempt {ticket.l1_attempts}/{self._config.l1_max_attempts}")
        self._save_tickets()

        # Fire triage event — MCP-connected AI agent picks this up
        await self._fire_event("itsm.ticket.triage", {
            **ticket.to_dict(),
            "tier": AGENT_L1,
            "model_config": self._config.l1_model.to_dict(),
            "timeout_seconds": self._config.l1_timeout_seconds,
        })

        # Start auto-escalation timer in case no agent responds
        self._triage_timers[ticket.id] = asyncio.create_task(
            self._triage_timeout(ticket.id, self._config.l1_timeout_seconds),
            name=f"itsm-l1-timeout-{ticket.id}",
        )

    async def _start_l2_triage(self, ticket: Ticket, notes: str = "") -> None:
        ticket.status = "l2_triage"
        ticket.agent_tier = AGENT_L2
        ticket.l2_attempts += 1
        ticket.add_audit(
            "system", "l2_triage_started",
            f"L1 exhausted. Notes: {notes[:200]}. "
            f"Attempt {ticket.l2_attempts}/{self._config.l2_max_attempts}",
        )
        self._save_tickets()

        await self._fire_event("itsm.ticket.triage", {
            **ticket.to_dict(),
            "tier": AGENT_L2,
            "model_config": self._config.l2_model.to_dict(),
            "timeout_seconds": self._config.l2_timeout_seconds,
            "l1_notes": notes,
        })

        self._triage_timers[ticket.id] = asyncio.create_task(
            self._triage_timeout(ticket.id, self._config.l2_timeout_seconds),
            name=f"itsm-l2-timeout-{ticket.id}",
        )

    async def _triage_timeout(self, ticket_id: str, timeout: int) -> None:
        """Auto-escalate when an agent tier times out without resolving."""
        await asyncio.sleep(timeout)
        ticket = self._tickets.get(ticket_id)
        if not ticket or ticket.status in ("resolved", "closed", "acknowledged"):
            return
        log.info("Ticket %s triage timeout (%s) — auto-escalating", ticket_id, ticket.agent_tier)
        ticket.add_audit("system", "triage_timeout",
                         f"{ticket.agent_tier} triage timed out after {timeout}s")
        await self.escalate_ticket(ticket_id, actor="system", notes="Triage timeout")

    # ── Human escalation ──────────────────────────────────────────────

    async def _escalate_to_human(self, ticket: Ticket, notes: str = "") -> None:
        ticket.status = "pending_human"
        ticket.agent_tier = AGENT_HUMAN
        ticket.escalation_tier_index = 0
        ticket.add_audit("system", "escalated_to_human",
                         f"Agent triage exhausted. Notes: {notes[:200]}")
        self._save_tickets()

        await self._fire_event("itsm.ticket.needs_human", ticket.to_dict())
        await self._push_to_external(ticket)
        await self._notify_escalation_tier(ticket)

    async def _advance_escalation_tier(self, ticket: Ticket) -> None:
        """Move to the next tier in the escalation policy after no ack."""
        policy = self._get_policy_for_ticket(ticket)
        if not policy:
            return
        ticket.escalation_tier_index += 1
        if ticket.escalation_tier_index >= len(policy.tiers):
            # Exhausted all tiers — log and give up escalating
            ticket.add_audit("system", "escalation_exhausted",
                             "All escalation tiers exhausted with no acknowledgment")
            self._save_tickets()
            await self._fire_event("itsm.ticket.escalation_exhausted", ticket.to_dict())
            return
        ticket.add_audit(
            "system", "escalation_advanced",
            f"No ack — advancing to tier {ticket.escalation_tier_index}",
        )
        self._save_tickets()
        await self._notify_escalation_tier(ticket)

    async def _notify_escalation_tier(self, ticket: Ticket) -> None:
        """Send notifications to users in the current escalation tier."""
        policy = self._get_policy_for_ticket(ticket)
        if not policy or not policy.tiers:
            # No policy — fire event and hope someone is watching the dashboard
            log.warning("No escalation policy for ticket %s", ticket.id)
            return

        tier_idx = ticket.escalation_tier_index
        if tier_idx >= len(policy.tiers):
            return

        tier = policy.tiers[tier_idx]
        notified_any = False

        for uid in tier.user_ids:
            oncall_user = self._config.oncall_users.get(uid)
            if not oncall_user:
                continue
            if oncall_user.is_available(ticket.priority):
                channels = tier.channels_override or oncall_user.channels
                await self._send_oncall_notification(ticket, oncall_user, channels)
                notified_any = True
            else:
                next_avail = oncall_user.next_available_at()
                log.info(
                    "Ticket %s: user %s not available (next: %s)",
                    ticket.id, uid,
                    datetime.datetime.fromtimestamp(next_avail).isoformat()
                    if next_avail else "unknown",
                )

        if not notified_any:
            log.warning(
                "Ticket %s tier %d: no available users — will retry at SLA 75%%",
                ticket.id, tier_idx,
            )

        # Start ack timeout — advance to next tier if no ack
        timeout = tier.ack_timeout_seconds
        self._escalation_timers[ticket.id] = asyncio.create_task(
            self._ack_timeout(ticket.id, timeout),
            name=f"itsm-ack-timeout-{ticket.id}",
        )

    async def _ack_timeout(self, ticket_id: str, timeout: int) -> None:
        """Auto-advance escalation tier if no ack within timeout."""
        await asyncio.sleep(timeout)
        ticket = self._tickets.get(ticket_id)
        if not ticket or ticket.status in ("resolved", "closed", "acknowledged"):
            return
        log.info(
            "Ticket %s: no acknowledgment after %ds — advancing escalation",
            ticket_id, timeout,
        )
        await self._advance_escalation_tier(ticket)

    async def _send_oncall_notification(
        self,
        ticket: Ticket,
        user: OnCallUser,
        channels: list[str],
    ) -> None:
        """Send ticket notification via the user's configured channels."""
        if not self._notifier:
            log.warning("No notifier — cannot notify %s for ticket %s",
                        user.user_id, ticket.id)
            return

        level = {"critical": "critical", "high": "warning"}.get(ticket.priority, "info")
        data = {
            "ticket_id": ticket.id,
            "priority": ticket.priority,
            "subject": ticket.subject,
            "description": ticket.description[:500],
            "node_id": ticket.node_id,
            "source": ticket.source,
            "sla_response_deadline": ticket.sla_response_deadline,
            "ack_url": f"/api/v1/itsm/tickets/{ticket.id}/acknowledge",
        }

        # Use named destinations if available; fall back to on_event broadcast
        dest_ids = channels if channels else []
        if dest_ids and hasattr(self._notifier, "_destinations"):
            for dest_id in dest_ids:
                dest = self._notifier._destinations.get(dest_id)
                if dest:
                    asyncio.create_task(
                        self._notifier._send(dest, "itsm.ticket.needs_human", data, level),
                        name=f"itsm-notify-{ticket.id}-{dest_id}",
                    )
        else:
            await self._notifier.on_event("itsm.ticket.needs_human", data)

    # ── SLA monitoring ────────────────────────────────────────────────

    async def start(self) -> None:
        self._tasks.append(asyncio.create_task(
            self._sla_monitor(), name="itsm-sla-monitor"
        ))
        # Re-arm triage timers for open triage tickets that survived a restart
        for ticket in self._tickets.values():
            if ticket.status == "l1_triage" and ticket.id not in self._triage_timers:
                elapsed = time.time() - ticket.created_at
                remaining = max(0, self._config.l1_timeout_seconds - elapsed)
                self._triage_timers[ticket.id] = asyncio.create_task(
                    self._triage_timeout(ticket.id, remaining),
                    name=f"itsm-l1-timeout-{ticket.id}",
                )
            elif ticket.status == "l2_triage" and ticket.id not in self._triage_timers:
                elapsed = time.time() - ticket.created_at
                remaining = max(0, self._config.l2_timeout_seconds - elapsed)
                self._triage_timers[ticket.id] = asyncio.create_task(
                    self._triage_timeout(ticket.id, remaining),
                    name=f"itsm-l2-timeout-{ticket.id}",
                )

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        for t in self._triage_timers.values():
            t.cancel()
        for t in self._escalation_timers.values():
            t.cancel()

    async def _sla_monitor(self) -> None:
        """Fire SLA warning events at 50%, 75%, and 100% of each deadline."""
        warned: dict[str, set[str]] = {}   # ticket_id → set of warning labels fired

        while True:
            await asyncio.sleep(60)
            now = time.time()
            for ticket in list(self._tickets.values()):
                if ticket.status in ("resolved", "closed"):
                    continue
                twarned = warned.setdefault(ticket.id, set())

                # Response SLA
                if ticket.sla_response_deadline and not ticket.responded_at:
                    frac = ticket.sla_response_fraction()
                    for thresh in _SLA_WARN_FRACTIONS:
                        label = f"resp_{thresh}"
                        if frac >= thresh and label not in twarned:
                            twarned.add(label)
                            await self._fire_event("itsm.sla.warning", {
                                "ticket_id": ticket.id,
                                "subject": ticket.subject,
                                "priority": ticket.priority,
                                "sla_type": "response",
                                "fraction": frac,
                                "breached": frac >= 1.0,
                            })

                # Resolution SLA
                if ticket.sla_resolution_deadline:
                    frac = ticket.sla_resolution_fraction()
                    for thresh in _SLA_WARN_FRACTIONS:
                        label = f"res_{thresh}"
                        if frac >= thresh and label not in twarned:
                            twarned.add(label)
                            await self._fire_event("itsm.sla.warning", {
                                "ticket_id": ticket.id,
                                "subject": ticket.subject,
                                "priority": ticket.priority,
                                "sla_type": "resolution",
                                "fraction": frac,
                                "breached": frac >= 1.0,
                            })
                            if frac >= 1.0 and ticket.status == "pending_human":
                                # SLA breached with no ack — re-notify
                                await self._notify_escalation_tier(ticket)

    # ── External ITSM integration ─────────────────────────────────────

    async def _push_to_external(self, ticket: Ticket) -> None:
        """Push ticket to external ITSM webhook (Jira, Freshservice, etc.)."""
        url = self._config.external_webhook_url
        if not url:
            return
        payload = json.dumps({"ticket": ticket.to_dict()}).encode()
        headers = {"Content-Type": "application/json", **self._config.external_webhook_headers}
        try:
            loop = asyncio.get_running_loop()
            def _do():
                req = urllib.request.Request(url, data=payload,
                                             headers=headers, method="POST")
                resp = urllib.request.urlopen(req, timeout=10)
                return resp.read()
            result = await loop.run_in_executor(None, _do)
            # Some ITSM systems return the external ticket ID
            try:
                body = json.loads(result)
                if ext_id := body.get("id") or body.get("key"):
                    ticket.external_ref = str(ext_id)
                    ticket.add_audit("system", "external_pushed", f"External ref: {ext_id}")
                    self._save_tickets()
            except Exception:
                pass
        except Exception as e:
            log.debug("External ITSM push failed: %s", e)

    # ── Helpers ───────────────────────────────────────────────────────

    async def _fire_event(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            await self._event_queue.put({"type": event_type, **data})

    def _cancel_timers(self, ticket_id: str) -> None:
        for d in (self._triage_timers, self._escalation_timers):
            task = d.pop(ticket_id, None)
            if task:
                task.cancel()

    def _get_policy_for_ticket(self, ticket: Ticket) -> EscalationPolicy | None:
        pid = self._config.default_policy_id
        return self._config.escalation_policies.get(pid) if pid else None

    def status(self) -> dict[str, Any]:
        """Return summary stats for the dashboard."""
        tickets = list(self._tickets.values())
        open_tix = [t for t in tickets if t.status not in ("resolved", "closed")]
        return {
            "total": len(tickets),
            "open": len(open_tix),
            "by_status": {s: sum(1 for t in open_tix if t.status == s)
                          for s in _TICKET_STATUSES},
            "by_priority": {p: sum(1 for t in open_tix if t.priority == p)
                            for p in _PRIORITIES},
            "sla_breached": sum(1 for t in open_tix if t.sla_resolution_breached),
            "pending_human": sum(1 for t in tickets if t.status == "pending_human"),
            "oncall_users": len(self._config.oncall_users),
            "escalation_policies": len(self._config.escalation_policies),
        }
