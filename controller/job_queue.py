# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Async job queue for agent/node task execution.

Supports:
  - Single-target jobs: run a command/script/package op on one node
  - Campaign fan-out: one logical operation spawns N child jobs (one per target node)
  - Offline deferral: pending jobs are held until the target node's config WS reconnects
  - Parallel dispatch: all connected targets receive their jobs immediately on campaign create
  - Deadline expiry: jobs that never get a chance to run are marked SKIPPED
  - Result ingestion: agent posts ack/progress/result back via REST or config WS messages

Dispatch flow
─────────────
1. create_job() / create_campaign() stores jobs in _jobs dict (and persists)
2. If the target node is currently connected (_dispatch_fn is set and node has WS),
   _dispatch_job() sends {"type": "job", ...} on the config WS immediately.
3. If the node is offline, the job stays PENDING.
4. When the node reconnects, api.py calls on_node_connected(node_id), which dispatches
   all PENDING jobs for that node.
5. Agent sends back job_ack → DISPATCHED, job_result → COMPLETED/FAILED.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Awaitable

log = logging.getLogger("ozma.job_queue")

DATA_DIR = Path(__file__).parent / "job_data"

# ── Enums ─────────────────────────────────────────────────────────────────────

class JobType(str, Enum):
    COMMAND         = "command"          # run a shell command
    SCRIPT          = "script"           # run an inline script body
    PACKAGE_INSTALL = "package_install"  # install packages (apt/dnf/winget)
    PACKAGE_REMOVE  = "package_remove"   # remove packages
    FILE_PUSH       = "file_push"        # push a file to the node
    REMEDIATION     = "remediation"      # named fix recipe
    HEALTH_CHECK    = "health_check"     # verify a condition, no side effects


class JobState(str, Enum):
    PENDING     = "pending"     # waiting for node to connect
    DISPATCHED  = "dispatched"  # sent to node, awaiting ack
    RUNNING     = "running"     # node acknowledged, executing
    COMPLETED   = "completed"   # finished successfully (exit_code 0)
    FAILED      = "failed"      # finished with error or non-zero exit
    CANCELLED   = "cancelled"   # cancelled by operator before completion
    TIMED_OUT   = "timed_out"   # exceeded execution timeout on node
    SKIPPED     = "skipped"     # deadline elapsed before node came online


class TargetScope(str, Enum):
    NODE          = "node"          # explicit list of node IDs
    ALL           = "all"           # every registered node at creation time
    MACHINE_CLASS = "machine_class" # nodes with matching machine_class
    LABEL         = "label"         # nodes whose hw field matches a label


# ── Spec ──────────────────────────────────────────────────────────────────────

@dataclass
class JobSpec:
    """What to execute — identical for every child job in a campaign."""
    type:             JobType          = JobType.COMMAND
    # command / script
    command:          str              = ""
    args:             list[str]        = field(default_factory=list)
    env:              dict[str, str]   = field(default_factory=dict)
    working_dir:      str              = ""
    timeout_seconds:  int              = 300
    run_as:           str              = ""       # user to run as (empty = current)
    # package operations
    packages:         list[str]        = field(default_factory=list)
    package_manager:  str              = "auto"   # auto | apt | dnf | winget | brew
    # file push
    dest_path:        str              = ""
    content_b64:      str              = ""       # base64-encoded file content
    file_mode:        str              = "0644"
    # remediation recipe
    recipe:           str              = ""
    recipe_params:    dict[str, Any]   = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "type": self.type.value,
            "command": self.command, "args": self.args, "env": self.env,
            "working_dir": self.working_dir,
            "timeout_seconds": self.timeout_seconds, "run_as": self.run_as,
            "packages": self.packages, "package_manager": self.package_manager,
            "dest_path": self.dest_path, "content_b64": self.content_b64,
            "file_mode": self.file_mode,
            "recipe": self.recipe, "recipe_params": self.recipe_params,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> JobSpec:
        return cls(
            type=JobType(d.get("type", "command")),
            command=d.get("command", ""), args=d.get("args", []),
            env=d.get("env", {}), working_dir=d.get("working_dir", ""),
            timeout_seconds=d.get("timeout_seconds", 300),
            run_as=d.get("run_as", ""),
            packages=d.get("packages", []),
            package_manager=d.get("package_manager", "auto"),
            dest_path=d.get("dest_path", ""),
            content_b64=d.get("content_b64", ""),
            file_mode=d.get("file_mode", "0644"),
            recipe=d.get("recipe", ""),
            recipe_params=d.get("recipe_params", {}),
        )


# ── Job ───────────────────────────────────────────────────────────────────────

@dataclass
class Job:
    """A unit of work targeting exactly one node."""
    id:              str
    name:            str
    spec:            JobSpec
    target_node_id:  str
    # Hierarchy
    campaign_id:     str | None      = None   # set if part of a campaign
    # State
    state:           JobState        = JobState.PENDING
    created_at:      float           = field(default_factory=time.time)
    dispatched_at:   float | None    = None
    started_at:      float | None    = None
    completed_at:    float | None    = None
    deadline:        float | None    = None   # epoch; None = never expires
    # Result
    exit_code:       int | None      = None
    stdout:          str             = ""
    stderr:          str             = ""
    error:           str             = ""
    progress:        int             = 0      # 0-100
    progress_msg:    str             = ""
    # Metadata
    created_by:      str             = ""
    tags:            dict[str, str]  = field(default_factory=dict)
    attempt:         int             = 1      # incremented on retry

    @property
    def terminal(self) -> bool:
        return self.state in (JobState.COMPLETED, JobState.FAILED,
                              JobState.CANCELLED, JobState.TIMED_OUT,
                              JobState.SKIPPED)

    @property
    def duration_seconds(self) -> float | None:
        if self.started_at and self.completed_at:
            return round(self.completed_at - self.started_at, 3)
        return None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "spec": self.spec.to_dict(),
            "target_node_id": self.target_node_id,
            "campaign_id": self.campaign_id,
            "state": self.state.value,
            "created_at": self.created_at,
            "dispatched_at": self.dispatched_at,
            "started_at": self.started_at,
            "completed_at": self.completed_at,
            "deadline": self.deadline,
            "exit_code": self.exit_code,
            "stdout": self.stdout, "stderr": self.stderr, "error": self.error,
            "progress": self.progress, "progress_msg": self.progress_msg,
            "created_by": self.created_by, "tags": self.tags,
            "attempt": self.attempt,
            "duration_seconds": self.duration_seconds,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Job:
        return cls(
            id=d["id"], name=d["name"],
            spec=JobSpec.from_dict(d.get("spec", {})),
            target_node_id=d["target_node_id"],
            campaign_id=d.get("campaign_id"),
            state=JobState(d.get("state", "pending")),
            created_at=d.get("created_at", 0.0),
            dispatched_at=d.get("dispatched_at"),
            started_at=d.get("started_at"),
            completed_at=d.get("completed_at"),
            deadline=d.get("deadline"),
            exit_code=d.get("exit_code"),
            stdout=d.get("stdout", ""), stderr=d.get("stderr", ""),
            error=d.get("error", ""),
            progress=d.get("progress", 0), progress_msg=d.get("progress_msg", ""),
            created_by=d.get("created_by", ""),
            tags=d.get("tags", {}),
            attempt=d.get("attempt", 1),
        )


# ── Campaign ──────────────────────────────────────────────────────────────────

@dataclass
class Campaign:
    """
    A fan-out operation: one logical job dispatched to multiple nodes.
    The campaign itself is metadata; each node gets its own child Job.
    """
    id:                   str
    name:                 str
    spec:                 JobSpec
    target_scope:         TargetScope
    target_labels:        list[str]        = field(default_factory=list)
    resolved_node_ids:    list[str]        = field(default_factory=list)
    child_job_ids:        list[str]        = field(default_factory=list)
    created_at:           float            = field(default_factory=time.time)
    deadline:             float | None     = None
    created_by:           str             = ""
    tags:                 dict[str, str]   = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "spec": self.spec.to_dict(),
            "target_scope": self.target_scope.value,
            "target_labels": self.target_labels,
            "resolved_node_ids": self.resolved_node_ids,
            "child_job_ids": self.child_job_ids,
            "created_at": self.created_at,
            "deadline": self.deadline,
            "created_by": self.created_by,
            "tags": self.tags,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Campaign:
        return cls(
            id=d["id"], name=d["name"],
            spec=JobSpec.from_dict(d.get("spec", {})),
            target_scope=TargetScope(d.get("target_scope", "node")),
            target_labels=d.get("target_labels", []),
            resolved_node_ids=d.get("resolved_node_ids", []),
            child_job_ids=d.get("child_job_ids", []),
            created_at=d.get("created_at", 0.0),
            deadline=d.get("deadline"),
            created_by=d.get("created_by", ""),
            tags=d.get("tags", {}),
        )


# ── JobQueue ──────────────────────────────────────────────────────────────────

# Re-dispatch deadline for DISPATCHED jobs that never got an ack (node crash)
_DISPATCHED_REQUEUE_SECONDS = 120
_DEADLINE_CHECK_INTERVAL    = 60
_MAX_STDOUT_BYTES           = 64 * 1024   # truncate to 64 KiB


class JobQueue:
    """
    Central job queue.  Dispatch function is injected by api.py so the queue
    has no direct dependency on WebSocket infrastructure.

        queue = JobQueue(data_dir=..., state_ref=state)
        queue.set_dispatch_fn(my_send_fn)   # called by api.py after build_app
    """

    def __init__(self, data_dir: Path = DATA_DIR,
                 state_ref: Any = None) -> None:
        self._dir = data_dir
        self._dir.mkdir(parents=True, exist_ok=True)
        self._state = state_ref

        self._jobs:      dict[str, Job]      = {}
        self._campaigns: dict[str, Campaign] = {}
        self._dispatch_fn: Callable[[str, dict], Awaitable[bool]] | None = None

        self._deadline_task: asyncio.Task | None = None
        self._requeue_task:  asyncio.Task | None = None

        self._load()

    def set_dispatch_fn(self,
                        fn: Callable[[str, dict], Awaitable[bool]]) -> None:
        """Injected by api.py — sends a JSON dict to node_id via config WS."""
        self._dispatch_fn = fn

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._deadline_task = asyncio.create_task(
            self._deadline_loop(), name="job-deadline-check"
        )
        self._requeue_task = asyncio.create_task(
            self._requeue_loop(), name="job-requeue-check"
        )
        pending = sum(1 for j in self._jobs.values()
                      if j.state == JobState.PENDING)
        log.info("Job queue started (%d jobs, %d pending)",
                 len(self._jobs), pending)

    async def stop(self) -> None:
        for task in (self._deadline_task, self._requeue_task):
            if task:
                task.cancel()

    # ── Job creation ──────────────────────────────────────────────────────────

    async def create_job(self, name: str, spec: JobSpec,
                         target_node_id: str,
                         deadline: float | None = None,
                         created_by: str = "",
                         tags: dict[str, str] | None = None,
                         campaign_id: str | None = None) -> Job:
        """Create a single-target job and dispatch immediately if node is connected."""
        job = Job(
            id=str(uuid.uuid4()), name=name, spec=spec,
            target_node_id=target_node_id,
            campaign_id=campaign_id,
            deadline=deadline, created_by=created_by,
            tags=tags or {},
        )
        self._jobs[job.id] = job
        self._save_jobs()
        log.info("Job created: %s → %s (%s)", job.name, target_node_id, job.id[:8])
        await self._try_dispatch(job)
        return job

    async def create_campaign(self, name: str, spec: JobSpec,
                               target_scope: TargetScope,
                               target_ids: list[str] | None = None,
                               target_labels: list[str] | None = None,
                               deadline: float | None = None,
                               created_by: str = "",
                               tags: dict[str, str] | None = None) -> Campaign:
        """
        Fan-out: resolve target nodes and create one child Job per node.
        Dispatch is attempted immediately for all connected nodes.
        """
        node_ids = self._resolve_targets(target_scope, target_ids, target_labels)
        if not node_ids:
            log.warning("Campaign '%s': no matching nodes for scope=%s labels=%s",
                        name, target_scope.value, target_labels)

        campaign = Campaign(
            id=str(uuid.uuid4()), name=name, spec=spec,
            target_scope=target_scope,
            target_labels=target_labels or [],
            resolved_node_ids=node_ids,
            deadline=deadline, created_by=created_by,
            tags=tags or {},
        )
        self._campaigns[campaign.id] = campaign

        for node_id in node_ids:
            job = await self.create_job(
                name=f"{name} [{node_id}]",
                spec=spec,
                target_node_id=node_id,
                deadline=deadline,
                created_by=created_by,
                tags=tags or {},
                campaign_id=campaign.id,
            )
            campaign.child_job_ids.append(job.id)

        self._save_campaigns()
        log.info("Campaign created: %s → %d nodes (%s)",
                 name, len(node_ids), campaign.id[:8])
        return campaign

    # ── Node connectivity ─────────────────────────────────────────────────────

    async def on_node_connected(self, node_id: str) -> int:
        """
        Called by api.py when a node's config WS connects.
        Dispatches all PENDING jobs for this node.
        Returns number of jobs dispatched.
        """
        pending = [
            j for j in self._jobs.values()
            if j.target_node_id == node_id and j.state == JobState.PENDING
        ]
        dispatched = 0
        for job in pending:
            if await self._try_dispatch(job):
                dispatched += 1
        if dispatched:
            log.info("Node %s connected — dispatched %d pending jobs",
                     node_id, dispatched)
        return dispatched

    # ── Result ingestion ──────────────────────────────────────────────────────

    async def handle_ack(self, job_id: str) -> bool:
        """Agent acknowledged receipt of a job → DISPATCHED."""
        job = self._jobs.get(job_id)
        if not job or job.state not in (JobState.PENDING, JobState.DISPATCHED):
            return False
        job.state = JobState.DISPATCHED
        job.dispatched_at = job.dispatched_at or time.time()
        self._save_jobs()
        return True

    async def handle_progress(self, job_id: str, progress: int,
                               message: str = "") -> bool:
        """Agent sent a progress update → RUNNING."""
        job = self._jobs.get(job_id)
        if not job or job.terminal:
            return False
        job.state = JobState.RUNNING
        job.started_at = job.started_at or time.time()
        job.progress = max(0, min(100, progress))
        job.progress_msg = message
        self._save_jobs()
        return True

    async def handle_result(self, job_id: str, exit_code: int,
                             stdout: str = "", stderr: str = "",
                             error: str = "") -> bool:
        """Agent sent final result → COMPLETED or FAILED."""
        job = self._jobs.get(job_id)
        if not job or job.terminal:
            return False
        job.completed_at = time.time()
        job.started_at = job.started_at or job.dispatched_at or job.created_at
        job.exit_code = exit_code
        job.stdout = stdout[:_MAX_STDOUT_BYTES]
        job.stderr = stderr[:_MAX_STDOUT_BYTES]
        job.error = error
        job.progress = 100
        job.state = JobState.COMPLETED if exit_code == 0 else JobState.FAILED
        self._save_jobs()
        log.info("Job %s finished: %s (exit=%s) on %s",
                 job.id[:8], job.state.value, exit_code, job.target_node_id)
        return True

    # ── Control ───────────────────────────────────────────────────────────────

    def cancel_job(self, job_id: str) -> bool:
        """Cancel a job if it hasn't reached a terminal state."""
        job = self._jobs.get(job_id)
        if not job or job.terminal:
            return False
        job.state = JobState.CANCELLED
        job.completed_at = time.time()
        self._save_jobs()
        return True

    def cancel_campaign(self, campaign_id: str) -> int:
        """Cancel all non-terminal child jobs of a campaign. Returns count cancelled."""
        campaign = self._campaigns.get(campaign_id)
        if not campaign:
            return 0
        count = sum(
            1 for jid in campaign.child_job_ids
            if self.cancel_job(jid)
        )
        return count

    async def retry_job(self, job_id: str) -> Job | None:
        """
        Clone a failed/cancelled job as a new PENDING job on the same node.
        Returns the new job.
        """
        original = self._jobs.get(job_id)
        if not original or original.state not in (JobState.FAILED, JobState.CANCELLED,
                                                   JobState.TIMED_OUT, JobState.SKIPPED):
            return None
        new_job = Job(
            id=str(uuid.uuid4()),
            name=original.name,
            spec=original.spec,
            target_node_id=original.target_node_id,
            campaign_id=original.campaign_id,
            deadline=original.deadline,
            created_by=original.created_by,
            tags=original.tags,
            attempt=original.attempt + 1,
        )
        self._jobs[new_job.id] = new_job
        # Update campaign's child list if applicable
        if original.campaign_id:
            campaign = self._campaigns.get(original.campaign_id)
            if campaign and job_id in campaign.child_job_ids:
                idx = campaign.child_job_ids.index(job_id)
                campaign.child_job_ids[idx] = new_job.id
                self._save_campaigns()
        self._save_jobs()
        await self._try_dispatch(new_job)
        return new_job

    # ── Queries ───────────────────────────────────────────────────────────────

    def get_job(self, job_id: str) -> Job | None:
        return self._jobs.get(job_id)

    def list_jobs(self, state: str | None = None,
                  node_id: str | None = None,
                  campaign_id: str | None = None,
                  limit: int = 200,
                  offset: int = 0) -> list[Job]:
        jobs = list(self._jobs.values())
        if state:
            jobs = [j for j in jobs if j.state.value == state]
        if node_id:
            jobs = [j for j in jobs if j.target_node_id == node_id]
        if campaign_id:
            jobs = [j for j in jobs if j.campaign_id == campaign_id]
        jobs.sort(key=lambda j: j.created_at, reverse=True)
        return jobs[offset:offset + limit]

    def get_campaign(self, campaign_id: str) -> Campaign | None:
        return self._campaigns.get(campaign_id)

    def list_campaigns(self, limit: int = 100, offset: int = 0) -> list[Campaign]:
        campaigns = sorted(self._campaigns.values(),
                           key=lambda c: c.created_at, reverse=True)
        return campaigns[offset:offset + limit]

    def campaign_summary(self, campaign_id: str) -> dict[str, Any] | None:
        """Aggregate status counts across all child jobs of a campaign."""
        campaign = self._campaigns.get(campaign_id)
        if not campaign:
            return None
        counts: dict[str, int] = {s.value: 0 for s in JobState}
        total = 0
        for jid in campaign.child_job_ids:
            job = self._jobs.get(jid)
            if job:
                counts[job.state.value] += 1
                total += 1

        # Derive campaign-level state
        if counts["cancelled"] == total:
            overall = "cancelled"
        elif counts["pending"] == total:
            overall = "pending"
        elif counts["completed"] == total:
            overall = "completed"
        elif counts["failed"] == total:
            overall = "failed"
        elif all(counts[s] == 0 for s in ("pending", "dispatched", "running")):
            overall = "partial"   # mix of completed + failed
        else:
            overall = "running"

        return {
            "campaign_id": campaign_id,
            "name": campaign.name,
            "total": total,
            "counts": counts,
            "state": overall,
            "created_at": campaign.created_at,
        }

    # ── Internal ──────────────────────────────────────────────────────────────

    async def _try_dispatch(self, job: Job) -> bool:
        """Attempt to dispatch job; returns True if sent, False if node offline."""
        if job.state != JobState.PENDING:
            return False
        if not self._dispatch_fn:
            return False
        if job.deadline and time.time() > job.deadline:
            job.state = JobState.SKIPPED
            job.completed_at = time.time()
            job.error = "Deadline elapsed before dispatch"
            self._save_jobs()
            return False
        return await self._dispatch_job(job)

    async def _dispatch_job(self, job: Job) -> bool:
        """Send job to node via dispatch_fn. Returns True on success."""
        if not self._dispatch_fn:
            return False
        msg = {
            "type": "job",
            "job_id": job.id,
            "name": job.name,
            "spec": job.spec.to_dict(),
            "attempt": job.attempt,
        }
        try:
            ok = await self._dispatch_fn(job.target_node_id, msg)
        except Exception as exc:
            log.warning("Dispatch exception for job %s → %s: %s",
                        job.id[:8], job.target_node_id, exc)
            ok = False
        if ok:
            job.state = JobState.DISPATCHED
            job.dispatched_at = time.time()
            self._save_jobs()
            log.debug("Job %s dispatched to %s", job.id[:8], job.target_node_id)
        return ok

    def _resolve_targets(self, scope: TargetScope,
                         ids: list[str] | None,
                         labels: list[str] | None) -> list[str]:
        """Resolve TargetScope + ids/labels against current node registry."""
        if not self._state:
            return list(ids or [])

        nodes = getattr(self._state, "nodes", {})

        match scope:
            case TargetScope.NODE:
                return [nid for nid in (ids or []) if nid in nodes]
            case TargetScope.ALL:
                return list(nodes.keys())
            case TargetScope.MACHINE_CLASS:
                want = set(labels or [])
                return [
                    nid for nid, node in nodes.items()
                    if getattr(node, "machine_class", "workstation") in want
                ]
            case TargetScope.LABEL:
                want = set(labels or [])
                return [
                    nid for nid, node in nodes.items()
                    if getattr(node, "hw", "") in want
                    or bool(want & set(getattr(node, "tags", {}).keys()))
                ]
            case _:
                return []

    async def _deadline_loop(self) -> None:
        """Periodically expire PENDING jobs whose deadline has passed."""
        while True:
            await asyncio.sleep(_DEADLINE_CHECK_INTERVAL)
            now = time.time()
            changed = False
            for job in self._jobs.values():
                if (job.state == JobState.PENDING
                        and job.deadline and now > job.deadline):
                    job.state = JobState.SKIPPED
                    job.completed_at = now
                    job.error = "Deadline elapsed — node never connected"
                    changed = True
                    log.info("Job %s SKIPPED (deadline) on %s",
                             job.id[:8], job.target_node_id)
            if changed:
                self._save_jobs()

    async def _requeue_loop(self) -> None:
        """
        Re-dispatch jobs stuck in DISPATCHED for too long — handles node crashes
        between dispatch and ack.
        """
        while True:
            await asyncio.sleep(_DISPATCHED_REQUEUE_SECONDS)
            now = time.time()
            for job in list(self._jobs.values()):
                if (job.state == JobState.DISPATCHED
                        and job.dispatched_at
                        and now - job.dispatched_at > _DISPATCHED_REQUEUE_SECONDS):
                    log.info("Job %s requeued (no ack from %s)",
                             job.id[:8], job.target_node_id)
                    job.state = JobState.PENDING
                    job.dispatched_at = None
                    self._save_jobs()
                    await self._try_dispatch(job)

    # ── Persistence ───────────────────────────────────────────────────────────

    def _save_jobs(self) -> None:
        try:
            (self._dir / "jobs.json").write_text(
                json.dumps([j.to_dict() for j in self._jobs.values()], indent=2)
            )
        except Exception as exc:
            log.warning("Failed to persist jobs: %s", exc)

    def _save_campaigns(self) -> None:
        try:
            (self._dir / "campaigns.json").write_text(
                json.dumps([c.to_dict() for c in self._campaigns.values()], indent=2)
            )
        except Exception as exc:
            log.warning("Failed to persist campaigns: %s", exc)

    def _save(self) -> None:
        self._save_jobs()
        self._save_campaigns()

    def _load(self) -> None:
        jobs_path     = self._dir / "jobs.json"
        campaign_path = self._dir / "campaigns.json"
        try:
            if jobs_path.exists():
                for d in json.loads(jobs_path.read_text()):
                    j = Job.from_dict(d)
                    self._jobs[j.id] = j
            if campaign_path.exists():
                for d in json.loads(campaign_path.read_text()):
                    c = Campaign.from_dict(d)
                    self._campaigns[c.id] = c
        except Exception as exc:
            log.warning("Failed to load job data: %s", exc)
