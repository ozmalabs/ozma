"""Unit tests for the YAML visual regression test runner."""

import pytest

pytestmark = pytest.mark.unit


class TestStepResult:
    def test_defaults(self):
        from test_runner import StepResult
        s = StepResult(name="test step")
        assert s.status == "pending"
        assert s.error == ""
        assert s.screenshot_path == ""

    def test_to_dict(self):
        from test_runner import StepResult
        s = StepResult(name="check", status="passed", started_at=100.0, completed_at=100.5)
        d = s.to_dict()
        assert d["name"] == "check"
        assert d["status"] == "passed"
        assert d["duration_ms"] == 500


class TestTestResult:
    def test_defaults(self):
        from test_runner import TestResult
        r = TestResult(id="t1", name="Test", node_id="n1")
        assert r.status == "pending"
        assert r.steps == []

    def test_to_dict_counts(self):
        from test_runner import TestResult, StepResult
        r = TestResult(id="t1", name="Test", node_id="n1", status="completed")
        r.steps = [
            StepResult(name="s1", status="passed"),
            StepResult(name="s2", status="failed", error="oops"),
            StepResult(name="s3", status="passed"),
        ]
        d = r.to_dict()
        assert d["passed"] == 2
        assert d["failed"] == 1
        assert d["total"] == 3


class TestTestRunner:
    def test_abort_nonexistent(self):
        from test_runner import TestRunner
        from unittest.mock import MagicMock
        runner = TestRunner(agent_engine=MagicMock())
        assert runner.abort("nonexistent-id") is False

    def test_list_results_empty(self):
        from test_runner import TestRunner
        from unittest.mock import MagicMock
        runner = TestRunner(agent_engine=MagicMock())
        results = runner.list_results()
        assert isinstance(results, list)

    def test_get_result_none(self):
        from test_runner import TestRunner
        from unittest.mock import MagicMock
        runner = TestRunner(agent_engine=MagicMock())
        assert runner.get_result("nonexistent") is None
