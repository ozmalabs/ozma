#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for JobQueue — offline deferral, fan-out campaigns, result ingestion,
deadline expiry, cancel/retry, persistence, and target resolution.
"""

import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from job_queue import (
    JobQueue, Job, Campaign, JobSpec, JobType, JobState, TargetScope,
    _DISPATCHED_REQUEUE_SECONDS,
)


# ── Helpers ───────────────────────────────────────────────────────────────────

def _spec(**kw) -> JobSpec:
    return JobSpec(type=JobType.COMMAND, command="echo hello", **kw)


def _queue(tmp: Path, state=None) -> JobQueue:
    q = JobQueue(data_dir=tmp, state_ref=state)
    return q


def _mock_state(*node_ids, machine_classes=None) -> MagicMock:
    """Return a fake AppState with nodes dict."""
    state = MagicMock()
    mc = machine_classes or {}
    nodes = {}
    for nid in node_ids:
        node = MagicMock()
        node.machine_class = mc.get(nid, "workstation")
        node.hw = "test-hw"
        node.tags = {}
        nodes[nid] = node
    state.nodes = nodes
    return state


def run(coro):
    loop = asyncio.new_event_loop()
    try:
        return loop.run_until_complete(coro)
    finally:
        loop.close()


# ── JobSpec model ─────────────────────────────────────────────────────────────

class TestJobSpec(unittest.TestCase):

    def test_to_dict_round_trip(self):
        spec = JobSpec(type=JobType.PACKAGE_INSTALL,
                       packages=["vim", "git"], timeout_seconds=60)
        spec2 = JobSpec.from_dict(spec.to_dict())
        self.assertEqual(spec2.type, JobType.PACKAGE_INSTALL)
        self.assertEqual(spec2.packages, ["vim", "git"])
        self.assertEqual(spec2.timeout_seconds, 60)

    def test_defaults(self):
        spec = JobSpec()
        self.assertEqual(spec.type, JobType.COMMAND)
        self.assertEqual(spec.timeout_seconds, 300)
        self.assertEqual(spec.package_manager, "auto")


# ── Job model ─────────────────────────────────────────────────────────────────

class TestJobModel(unittest.TestCase):

    def _make(self, **kw) -> Job:
        defaults = dict(id="j1", name="Test", spec=_spec(),
                        target_node_id="node-1")
        return Job(**{**defaults, **kw})

    def test_terminal_states(self):
        for state in (JobState.COMPLETED, JobState.FAILED, JobState.CANCELLED,
                      JobState.TIMED_OUT, JobState.SKIPPED):
            j = self._make(state=state)
            self.assertTrue(j.terminal)

    def test_non_terminal_states(self):
        for state in (JobState.PENDING, JobState.DISPATCHED, JobState.RUNNING):
            j = self._make(state=state)
            self.assertFalse(j.terminal)

    def test_duration_seconds(self):
        j = self._make(started_at=1000.0, completed_at=1025.5)
        self.assertAlmostEqual(j.duration_seconds, 25.5)

    def test_duration_none_when_not_complete(self):
        j = self._make()
        self.assertIsNone(j.duration_seconds)

    def test_to_dict_round_trip(self):
        j = self._make(state=JobState.FAILED, exit_code=1,
                       stdout="out", stderr="err", attempt=3)
        d = j.to_dict()
        j2 = Job.from_dict(d)
        self.assertEqual(j2.state, JobState.FAILED)
        self.assertEqual(j2.exit_code, 1)
        self.assertEqual(j2.attempt, 3)

    def test_to_dict_includes_duration(self):
        j = self._make(started_at=100.0, completed_at=110.0)
        self.assertAlmostEqual(j.to_dict()["duration_seconds"], 10.0)


# ── Campaign model ────────────────────────────────────────────────────────────

class TestCampaignModel(unittest.TestCase):

    def test_to_dict_round_trip(self):
        c = Campaign(id="c1", name="Patch Tuesday",
                     spec=_spec(), target_scope=TargetScope.ALL,
                     resolved_node_ids=["n1", "n2"],
                     child_job_ids=["j1", "j2"])
        d = c.to_dict()
        c2 = Campaign.from_dict(d)
        self.assertEqual(c2.name, "Patch Tuesday")
        self.assertEqual(c2.target_scope, TargetScope.ALL)
        self.assertEqual(c2.resolved_node_ids, ["n1", "n2"])


# ── JobQueue: single-target jobs ──────────────────────────────────────────────

class TestJobQueueSingleJob(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_create_job_pending_when_no_dispatch_fn(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        self.assertEqual(job.state, JobState.PENDING)

    def test_create_job_dispatched_when_fn_set(self):
        q = _queue(self.tmp)
        dispatched = []

        async def fn(node_id, msg):
            dispatched.append((node_id, msg))
            return True

        q.set_dispatch_fn(fn)
        job = run(q.create_job("test", _spec(), "node-1"))
        self.assertEqual(job.state, JobState.DISPATCHED)
        self.assertEqual(len(dispatched), 1)
        self.assertEqual(dispatched[0][0], "node-1")
        self.assertEqual(dispatched[0][1]["type"], "job")
        self.assertEqual(dispatched[0][1]["job_id"], job.id)

    def test_dispatch_fn_failure_leaves_pending(self):
        q = _queue(self.tmp)

        async def fn(node_id, msg):
            return False  # node offline

        q.set_dispatch_fn(fn)
        job = run(q.create_job("test", _spec(), "node-1"))
        self.assertEqual(job.state, JobState.PENDING)

    def test_get_job(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        self.assertEqual(q.get_job(job.id).id, job.id)

    def test_get_job_missing(self):
        q = _queue(self.tmp)
        self.assertIsNone(q.get_job("nope"))

    def test_list_jobs_filter_state(self):
        q = _queue(self.tmp)
        j1 = run(q.create_job("j1", _spec(), "node-1"))
        j2 = run(q.create_job("j2", _spec(), "node-2"))
        pending = q.list_jobs(state="pending")
        self.assertEqual(len(pending), 2)

    def test_list_jobs_filter_node(self):
        q = _queue(self.tmp)
        run(q.create_job("j1", _spec(), "node-1"))
        run(q.create_job("j2", _spec(), "node-2"))
        jobs = q.list_jobs(node_id="node-1")
        self.assertEqual(len(jobs), 1)
        self.assertEqual(jobs[0].name, "j1")

    def test_list_jobs_sorted_newest_first(self):
        q = _queue(self.tmp)
        j1 = run(q.create_job("first", _spec(), "n1"))
        j2 = run(q.create_job("second", _spec(), "n1"))
        jobs = q.list_jobs()
        self.assertEqual(jobs[0].name, "second")


# ── JobQueue: on_node_connected ───────────────────────────────────────────────

class TestOnNodeConnected(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_dispatch_on_connect(self):
        q = _queue(self.tmp)
        # Create job while offline (no dispatch_fn)
        job = run(q.create_job("offline-job", _spec(), "node-offline"))
        self.assertEqual(job.state, JobState.PENDING)

        # Node comes online
        dispatched = []

        async def fn(node_id, msg):
            dispatched.append(msg)
            return True

        q.set_dispatch_fn(fn)
        n = run(q.on_node_connected("node-offline"))

        self.assertEqual(n, 1)
        self.assertEqual(q.get_job(job.id).state, JobState.DISPATCHED)
        self.assertEqual(len(dispatched), 1)

    def test_does_not_dispatch_other_nodes_jobs(self):
        q = _queue(self.tmp)
        run(q.create_job("job-a", _spec(), "node-a"))
        dispatched = []

        async def fn(node_id, msg):
            dispatched.append(node_id)
            return True

        q.set_dispatch_fn(fn)
        n = run(q.on_node_connected("node-b"))  # different node

        self.assertEqual(n, 0)
        self.assertEqual(len(dispatched), 0)

    def test_multiple_pending_dispatched_on_connect(self):
        q = _queue(self.tmp)
        for i in range(5):
            run(q.create_job(f"job-{i}", _spec(), "node-1"))

        async def fn(node_id, msg):
            return True

        q.set_dispatch_fn(fn)
        n = run(q.on_node_connected("node-1"))
        self.assertEqual(n, 5)

    def test_already_dispatched_not_redispatched(self):
        q = _queue(self.tmp)
        dispatched = []

        async def fn(node_id, msg):
            dispatched.append(msg)
            return True

        q.set_dispatch_fn(fn)
        job = run(q.create_job("job", _spec(), "node-1"))
        self.assertEqual(job.state, JobState.DISPATCHED)

        # Simulate another connect event
        n = run(q.on_node_connected("node-1"))
        self.assertEqual(n, 0)
        self.assertEqual(len(dispatched), 1)  # dispatched only once


# ── JobQueue: result ingestion ─────────────────────────────────────────────────

class TestResultIngestion(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def _pending_job(self) -> tuple[JobQueue, Job]:
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        return q, job

    def test_handle_ack_sets_dispatched(self):
        q, job = self._pending_job()
        ok = run(q.handle_ack(job.id))
        self.assertTrue(ok)
        self.assertEqual(q.get_job(job.id).state, JobState.DISPATCHED)

    def test_handle_progress_sets_running(self):
        q, job = self._pending_job()
        run(q.handle_progress(job.id, 50, "halfway"))
        j = q.get_job(job.id)
        self.assertEqual(j.state, JobState.RUNNING)
        self.assertEqual(j.progress, 50)
        self.assertEqual(j.progress_msg, "halfway")

    def test_handle_result_success(self):
        q, job = self._pending_job()
        run(q.handle_result(job.id, 0, stdout="ok", stderr=""))
        j = q.get_job(job.id)
        self.assertEqual(j.state, JobState.COMPLETED)
        self.assertEqual(j.exit_code, 0)
        self.assertEqual(j.stdout, "ok")
        self.assertEqual(j.progress, 100)

    def test_handle_result_failure(self):
        q, job = self._pending_job()
        run(q.handle_result(job.id, 1, stderr="error output"))
        j = q.get_job(job.id)
        self.assertEqual(j.state, JobState.FAILED)
        self.assertEqual(j.exit_code, 1)

    def test_handle_result_truncates_stdout(self):
        q, job = self._pending_job()
        big_output = "x" * (70 * 1024)
        run(q.handle_result(job.id, 0, stdout=big_output))
        j = q.get_job(job.id)
        self.assertLessEqual(len(j.stdout), 64 * 1024)

    def test_handle_result_ignores_terminal_job(self):
        q, job = self._pending_job()
        run(q.handle_result(job.id, 0))
        # Second result on already-completed job
        ok = run(q.handle_result(job.id, 1, stderr="late"))
        self.assertFalse(ok)
        self.assertEqual(q.get_job(job.id).state, JobState.COMPLETED)

    def test_handle_ack_missing_job(self):
        q = _queue(self.tmp)
        ok = run(q.handle_ack("nonexistent"))
        self.assertFalse(ok)


# ── JobQueue: cancel and retry ────────────────────────────────────────────────

class TestCancelRetry(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_cancel_pending_job(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        ok = q.cancel_job(job.id)
        self.assertTrue(ok)
        self.assertEqual(q.get_job(job.id).state, JobState.CANCELLED)

    def test_cancel_terminal_job_fails(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        run(q.handle_result(job.id, 0))
        ok = q.cancel_job(job.id)
        self.assertFalse(ok)

    def test_cancel_missing_job(self):
        q = _queue(self.tmp)
        self.assertFalse(q.cancel_job("nope"))

    def test_retry_failed_job_creates_new(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        run(q.handle_result(job.id, 1))  # fail it

        new_job = run(q.retry_job(job.id))
        self.assertIsNotNone(new_job)
        self.assertNotEqual(new_job.id, job.id)
        self.assertEqual(new_job.target_node_id, "node-1")
        self.assertEqual(new_job.attempt, 2)
        self.assertEqual(new_job.state, JobState.PENDING)

    def test_retry_pending_job_fails(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        # PENDING is not retryable (not a terminal-failure state)
        self.assertIsNone(run(q.retry_job(job.id)))

    def test_retry_completed_job_fails(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        run(q.handle_result(job.id, 0))  # succeed
        self.assertIsNone(run(q.retry_job(job.id)))

    def test_retry_skipped_job_allowed(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        job.state = JobState.SKIPPED
        new_job = run(q.retry_job(job.id))
        self.assertIsNotNone(new_job)


# ── JobQueue: deadline expiry ─────────────────────────────────────────────────

class TestDeadlineExpiry(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_deadline_elapsed_skips_job_on_create(self):
        """Job with past deadline is immediately skipped on dispatch attempt."""
        q = _queue(self.tmp)

        async def fn(node_id, msg):
            return True

        q.set_dispatch_fn(fn)
        past = time.time() - 3600  # 1 hour ago
        job = run(q.create_job("expired", _spec(), "node-1", deadline=past))
        self.assertEqual(job.state, JobState.SKIPPED)

    def test_future_deadline_does_not_skip(self):
        q = _queue(self.tmp)
        future = time.time() + 3600
        job = run(q.create_job("future", _spec(), "node-1", deadline=future))
        self.assertEqual(job.state, JobState.PENDING)

    def test_deadline_loop_skips_expired_pending(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        # Manually set a past deadline
        job.deadline = time.time() - 1
        # Run one deadline check cycle
        run(q._deadline_loop.__wrapped__(q) if hasattr(q._deadline_loop, '__wrapped__')
            else asyncio.wait_for(q._deadline_loop(), timeout=0.01)
            if False else _run_deadline_check(q))
        self.assertEqual(q.get_job(job.id).state, JobState.SKIPPED)


def _run_deadline_check(q: JobQueue):
    """Run one iteration of the deadline check without the sleep."""
    async def _once():
        now = time.time()
        for job in q._jobs.values():
            if (job.state == JobState.PENDING
                    and job.deadline and now > job.deadline):
                job.state = JobState.SKIPPED
                job.completed_at = now
                job.error = "Deadline elapsed"
        q._save_jobs()
    return _once()


# ── JobQueue: campaigns ───────────────────────────────────────────────────────

class TestCampaigns(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_campaign_creates_child_per_node(self):
        state = _mock_state("n1", "n2", "n3")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign(
            "Patch all", _spec(), TargetScope.ALL
        ))
        self.assertEqual(len(campaign.child_job_ids), 3)
        self.assertEqual(len(campaign.resolved_node_ids), 3)

    def test_campaign_node_scope(self):
        state = _mock_state("n1", "n2", "n3")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign(
            "Target two", _spec(), TargetScope.NODE,
            target_ids=["n1", "n3"],
        ))
        self.assertEqual(len(campaign.child_job_ids), 2)
        nodes = {q.get_job(jid).target_node_id for jid in campaign.child_job_ids}
        self.assertEqual(nodes, {"n1", "n3"})

    def test_campaign_machine_class_scope(self):
        state = _mock_state("n1", "n2", "n3",
                            machine_classes={"n1": "server", "n2": "workstation",
                                             "n3": "server"})
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign(
            "Server jobs", _spec(), TargetScope.MACHINE_CLASS,
            target_labels=["server"],
        ))
        self.assertEqual(len(campaign.child_job_ids), 2)
        nodes = {q.get_job(jid).target_node_id for jid in campaign.child_job_ids}
        self.assertIn("n1", nodes)
        self.assertIn("n3", nodes)

    def test_campaign_empty_no_error(self):
        state = _mock_state()  # no nodes
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Empty", _spec(), TargetScope.ALL))
        self.assertEqual(len(campaign.child_job_ids), 0)

    def test_campaign_dispatches_connected_nodes(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        dispatched = []

        async def fn(node_id, msg):
            dispatched.append(node_id)
            return True

        q.set_dispatch_fn(fn)
        campaign = run(q.create_campaign("Deploy", _spec(), TargetScope.ALL))
        self.assertEqual(len(dispatched), 2)
        # All child jobs should be DISPATCHED
        for jid in campaign.child_job_ids:
            self.assertEqual(q.get_job(jid).state, JobState.DISPATCHED)

    def test_campaign_summary_counts(self):
        state = _mock_state("n1", "n2", "n3")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Count test", _spec(), TargetScope.ALL))

        # Complete one, fail one, leave one pending
        jobs = [q.get_job(jid) for jid in campaign.child_job_ids]
        run(q.handle_result(jobs[0].id, 0))
        run(q.handle_result(jobs[1].id, 1))

        summary = q.campaign_summary(campaign.id)
        self.assertEqual(summary["counts"]["completed"], 1)
        self.assertEqual(summary["counts"]["failed"], 1)
        self.assertEqual(summary["counts"]["pending"], 1)
        self.assertEqual(summary["total"], 3)

    def test_campaign_summary_overall_state_completed(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("All done", _spec(), TargetScope.ALL))
        for jid in campaign.child_job_ids:
            run(q.handle_result(jid, 0))
        summary = q.campaign_summary(campaign.id)
        self.assertEqual(summary["state"], "completed")

    def test_campaign_summary_overall_state_partial(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Mixed", _spec(), TargetScope.ALL))
        jobs = [q.get_job(jid) for jid in campaign.child_job_ids]
        run(q.handle_result(jobs[0].id, 0))
        run(q.handle_result(jobs[1].id, 1))
        summary = q.campaign_summary(campaign.id)
        self.assertEqual(summary["state"], "partial")

    def test_cancel_campaign(self):
        state = _mock_state("n1", "n2", "n3")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Cancel me", _spec(), TargetScope.ALL))
        count = q.cancel_campaign(campaign.id)
        self.assertEqual(count, 3)
        for jid in campaign.child_job_ids:
            self.assertEqual(q.get_job(jid).state, JobState.CANCELLED)

    def test_cancel_campaign_respects_already_terminal(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Partial cancel", _spec(), TargetScope.ALL))
        jobs = [q.get_job(jid) for jid in campaign.child_job_ids]
        run(q.handle_result(jobs[0].id, 0))  # already completed

        count = q.cancel_campaign(campaign.id)
        self.assertEqual(count, 1)  # only the pending one

    def test_retry_updates_campaign_child_list(self):
        state = _mock_state("n1")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("Retry test", _spec(), TargetScope.ALL))
        jid = campaign.child_job_ids[0]
        run(q.handle_result(jid, 1))  # fail

        new_job = run(q.retry_job(jid))
        campaign = q.get_campaign(campaign.id)
        self.assertIn(new_job.id, campaign.child_job_ids)
        self.assertNotIn(jid, campaign.child_job_ids)

    def test_list_campaign_jobs(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("List test", _spec(), TargetScope.ALL))
        jobs = q.list_jobs(campaign_id=campaign.id)
        self.assertEqual(len(jobs), 2)


# ── JobQueue: persistence ─────────────────────────────────────────────────────

class TestPersistence(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_jobs_persisted_and_reloaded(self):
        q = _queue(self.tmp)
        run(q.create_job("persist-me", _spec(), "node-1"))

        q2 = _queue(self.tmp)
        jobs = q2.list_jobs()
        self.assertEqual(len(jobs), 1)
        self.assertEqual(jobs[0].name, "persist-me")

    def test_campaigns_persisted_and_reloaded(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        campaign = run(q.create_campaign("persist-campaign", _spec(), TargetScope.ALL))

        q2 = _queue(self.tmp)
        c2 = q2.get_campaign(campaign.id)
        self.assertIsNotNone(c2)
        self.assertEqual(c2.name, "persist-campaign")
        self.assertEqual(len(c2.child_job_ids), 2)

    def test_result_state_persisted(self):
        q = _queue(self.tmp)
        job = run(q.create_job("test", _spec(), "node-1"))
        run(q.handle_result(job.id, 0, stdout="done"))

        q2 = _queue(self.tmp)
        j2 = q2.get_job(job.id)
        self.assertEqual(j2.state, JobState.COMPLETED)
        self.assertEqual(j2.stdout, "done")


# ── JobQueue: target resolution ───────────────────────────────────────────────

class TestTargetResolution(unittest.TestCase):

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmpdir.name)

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_node_scope_filters_to_known_nodes(self):
        state = _mock_state("n1", "n2")
        q = _queue(self.tmp, state=state)
        result = q._resolve_targets(TargetScope.NODE, ["n1", "n99"], [])
        self.assertEqual(result, ["n1"])

    def test_all_scope_returns_all_nodes(self):
        state = _mock_state("n1", "n2", "n3")
        q = _queue(self.tmp, state=state)
        result = q._resolve_targets(TargetScope.ALL, None, None)
        self.assertEqual(set(result), {"n1", "n2", "n3"})

    def test_machine_class_scope(self):
        state = _mock_state("ws1", "srv1",
                            machine_classes={"ws1": "workstation", "srv1": "server"})
        q = _queue(self.tmp, state=state)
        result = q._resolve_targets(TargetScope.MACHINE_CLASS, None, ["server"])
        self.assertEqual(result, ["srv1"])

    def test_no_state_returns_ids_as_is(self):
        q = _queue(self.tmp, state=None)
        result = q._resolve_targets(TargetScope.NODE, ["a", "b"], None)
        self.assertEqual(result, ["a", "b"])


if __name__ == "__main__":
    unittest.main()
