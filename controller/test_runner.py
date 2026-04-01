# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Visual regression test runner.

Executes YAML-defined test suites against machines on the ozma mesh.
Each test step uses the agent engine to interact with a machine and
verify screen state through OCR, element detection, and screenshot
comparison.

Test format:

    name: "Windows 10 install"
    timeout: 3600
    node: "test-bench-1"
    steps:
      - name: "Installer starts"
        wait_for_text: "Windows Setup"
        timeout: 300
        screenshot: true
      - name: "Click Next"
        click:
          text: "Next"
        assert_text: "license terms"
      - name: "Accept license"
        click:
          element_type: checkbox
        click:
          text: "Next"

API:
  POST /api/v1/tests/run     — run a test file or inline YAML
  GET  /api/v1/tests/{id}    — get test result
  GET  /api/v1/tests/history  — past results
  POST /api/v1/tests/abort/{id} — abort running test
"""

from __future__ import annotations

import asyncio
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.test_runner")

RESULTS_DIR = Path(__file__).parent / "static" / "test_results"


@dataclass
class StepResult:
    """Result of a single test step."""
    name: str
    status: str = "pending"  # pending, passed, failed, skipped
    started_at: float = 0.0
    completed_at: float = 0.0
    screenshot_path: str = ""
    error: str = ""
    screen_text: str = ""

    def to_dict(self) -> dict:
        return {
            "name": self.name, "status": self.status,
            "duration_ms": int((self.completed_at - self.started_at) * 1000) if self.started_at else 0,
            "error": self.error,
            "screenshot": self.screenshot_path,
        }


@dataclass
class TestResult:
    """Result of a complete test run."""
    id: str
    name: str
    node_id: str
    status: str = "pending"  # pending, running, passed, failed, aborted
    started_at: float = 0.0
    completed_at: float = 0.0
    steps: list[StepResult] = field(default_factory=list)
    error: str = ""

    def to_dict(self) -> dict:
        return {
            "id": self.id, "name": self.name, "node": self.node_id,
            "status": self.status,
            "duration_s": round(
                (self.completed_at or time.time()) - self.started_at, 1
            ) if self.started_at else 0,
            "steps": [s.to_dict() for s in self.steps],
            "passed": sum(1 for s in self.steps if s.status == "passed"),
            "failed": sum(1 for s in self.steps if s.status == "failed"),
            "total": len(self.steps),
            "error": self.error,
        }


class TestRunner:
    """
    Visual regression test executor.

    Runs YAML test definitions using the agent engine for screen interaction
    and verification.
    """

    def __init__(self, agent_engine: Any, notifier: Any = None) -> None:
        self._agent = agent_engine
        self._notifier = notifier
        self._active: dict[str, TestResult] = {}
        self._history: list[TestResult] = []
        self._abort_flags: dict[str, asyncio.Event] = {}
        RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    async def run_file(self, path: str, node_id: str = "") -> dict:
        """Load and run a YAML test file."""
        import yaml
        test_path = Path(path)
        if not test_path.exists():
            return {"error": f"Test file not found: {path}"}
        data = yaml.safe_load(test_path.read_text())
        return await self.run(data, node_id)

    async def run(self, test_def: dict, node_id: str = "") -> dict:
        """Run a test from a parsed YAML definition."""
        test_id = f"test-{uuid.uuid4().hex[:8]}"
        name = test_def.get("name", test_id)
        node = node_id or test_def.get("node", "")
        overall_timeout = test_def.get("timeout", 3600)

        result = TestResult(
            id=test_id, name=name, node_id=node,
            status="running", started_at=time.time(),
        )
        self._active[test_id] = result
        abort_event = asyncio.Event()
        self._abort_flags[test_id] = abort_event

        results_dir = RESULTS_DIR / test_id
        results_dir.mkdir(parents=True, exist_ok=True)

        try:
            # Setup steps
            for setup_step in test_def.get("setup", []):
                await self._run_setup_step(setup_step, node, result)

            # Test steps
            steps = test_def.get("steps", [])
            for i, step_def in enumerate(steps):
                if abort_event.is_set():
                    result.status = "aborted"
                    break

                step_name = step_def.get("name", f"Step {i + 1}")
                step_result = StepResult(name=step_name, started_at=time.time())
                result.steps.append(step_result)

                try:
                    await asyncio.wait_for(
                        self._run_step(step_def, node, step_result, results_dir),
                        timeout=step_def.get("timeout", overall_timeout),
                    )
                    if step_result.status == "pending":
                        step_result.status = "passed"
                except asyncio.TimeoutError:
                    step_result.status = "failed"
                    step_result.error = f"Timeout after {step_def.get('timeout', overall_timeout)}s"
                except Exception as e:
                    step_result.status = "failed"
                    step_result.error = str(e)

                step_result.completed_at = time.time()

                if step_result.status == "failed":
                    # Capture failure screenshot
                    fail_shot = await self._agent.execute(
                        "screenshot", node_id=node
                    )
                    if fail_shot.screenshot_base64:
                        import base64
                        fail_path = results_dir / f"FAIL_{i}_{step_name}.jpg"
                        fail_path.write_bytes(base64.b64decode(fail_shot.screenshot_base64))
                        step_result.screenshot_path = str(fail_path)

                    # Run on_failure actions
                    for fail_action in test_def.get("on_failure", []):
                        await self._run_setup_step(fail_action, node, result)
                    break  # Stop on first failure

            # Determine overall result
            if result.status == "running":
                if any(s.status == "failed" for s in result.steps):
                    result.status = "failed"
                else:
                    result.status = "passed"

        except Exception as e:
            result.status = "failed"
            result.error = str(e)
            log.error("Test %s failed: %s", test_id, e)

        finally:
            result.completed_at = time.time()
            del self._active[test_id]
            del self._abort_flags[test_id]
            self._history.append(result)

            # Notify on failure
            if result.status == "failed" and self._notifier:
                await self._notifier.on_event("test.failed", {
                    "test": name, "node": node,
                    "error": result.error or next(
                        (s.error for s in result.steps if s.status == "failed"), ""
                    ),
                })

            # Save results
            import json
            (results_dir / "result.json").write_text(
                json.dumps(result.to_dict(), indent=2)
            )

        return result.to_dict()

    async def _run_step(self, step_def: dict, node: str,
                         step_result: StepResult, results_dir: Path) -> None:
        """Execute a single test step."""
        # ── wait_for_text ─────────────────────────────────────────────
        if "wait_for_text" in step_def:
            text = step_def["wait_for_text"]
            timeout = step_def.get("timeout", 60)
            r = await self._agent.execute(
                "wait_for_text", node_id=node, text=text, timeout=timeout
            )
            if not r.success:
                step_result.status = "failed"
                step_result.error = r.error or f"Text not found: {text}"
                return
            step_result.screen_text = r.screen_text

        # ── click (by text, coordinates, or element) ──────────────────
        if "click" in step_def:
            click_spec = step_def["click"]
            if isinstance(click_spec, dict):
                if "text" in click_spec:
                    # Find element by text, then click
                    r = await self._agent.execute(
                        "find_elements", node_id=node, som=True
                    )
                    if r.success:
                        target_text = click_spec["text"].lower()
                        for eid, el in r.som_elements.items():
                            if target_text in el.get("text", "").lower():
                                await self._agent.execute(
                                    "click", node_id=node, element_id=eid
                                )
                                break
                        else:
                            step_result.status = "failed"
                            step_result.error = f"Click target not found: {click_spec['text']}"
                            return
                elif "x" in click_spec and "y" in click_spec:
                    await self._agent.execute(
                        "click", node_id=node,
                        x=click_spec["x"], y=click_spec["y"]
                    )
                elif "element_id" in click_spec:
                    await self._agent.execute(
                        "click", node_id=node,
                        element_id=click_spec["element_id"]
                    )
                elif "element_type" in click_spec:
                    # Find first element of this type and click it
                    r = await self._agent.execute(
                        "find_elements", node_id=node, som=True
                    )
                    if r.success:
                        for eid, el in r.som_elements.items():
                            if el.get("type") == click_spec["element_type"]:
                                await self._agent.execute(
                                    "click", node_id=node, element_id=eid
                                )
                                break

        # ── type ──────────────────────────────────────────────────────
        if "type" in step_def:
            await self._agent.execute("type", node_id=node, text=step_def["type"])

        # ── key ───────────────────────────────────────────────────────
        if "key" in step_def:
            await self._agent.execute("key", node_id=node, key=step_def["key"])

        # ── hotkey ────────────────────────────────────────────────────
        if "hotkey" in step_def:
            keys = step_def["hotkey"]
            if isinstance(keys, str):
                keys = keys.split("+")
            await self._agent.execute("hotkey", node_id=node, keys=keys)

        # ── wait ──────────────────────────────────────────────────────
        if "wait" in step_def:
            await asyncio.sleep(float(step_def["wait"]))

        # ── assert_text ───────────────────────────────────────────────
        if "assert_text" in step_def:
            r = await self._agent.execute(
                "assert_text", node_id=node, text=step_def["assert_text"]
            )
            if not r.success:
                step_result.status = "failed"
                step_result.error = r.error or f"Assert failed: {step_def['assert_text']}"
                return

        # ── assert_not_present ────────────────────────────────────────
        if "assert_not_present" in step_def:
            r = await self._agent.execute(
                "read_screen", node_id=node
            )
            if r.success and step_def["assert_not_present"].lower() in r.screen_text.lower():
                step_result.status = "failed"
                step_result.error = f"Unexpected text found: {step_def['assert_not_present']}"
                return

        # ── assert_element ────────────────────────────────────────────
        if "assert_element" in step_def:
            spec = step_def["assert_element"]
            r = await self._agent.execute(
                "assert_element", node_id=node,
                element_type=spec.get("type", ""),
                description=spec.get("description", ""),
            )
            if not r.success:
                step_result.status = "failed"
                step_result.error = r.error or f"Element not found"
                return

        # ── screenshot ────────────────────────────────────────────────
        if step_def.get("screenshot"):
            r = await self._agent.execute("screenshot", node_id=node)
            if r.screenshot_base64:
                import base64
                ss_path = results_dir / f"{step_result.name.replace(' ', '_')}.jpg"
                ss_path.write_bytes(base64.b64decode(r.screenshot_base64))
                step_result.screenshot_path = str(ss_path)

    async def _run_setup_step(self, step: dict, node: str, result: TestResult) -> None:
        """Run a setup/teardown action."""
        action = step.get("action", "")
        if action == "screenshot":
            r = await self._agent.execute("screenshot", node_id=node)
            if r.screenshot_base64:
                import base64
                path = step.get("path", f"test_results/{result.id}/setup.jpg")
                path = path.replace("{test_name}", result.name).replace(
                    "{timestamp}", str(int(time.time()))
                )
                p = Path(path)
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_bytes(base64.b64decode(r.screenshot_base64))
        elif action == "notify" and self._notifier:
            msg = step.get("message", "").replace("{test_name}", result.name)
            await self._notifier.on_event("test.notification", {
                "message": msg, "channel": step.get("channel", ""),
            })
        elif action == "key":
            await self._agent.execute("key", node_id=node, key=step.get("key", ""))
        elif action == "wait":
            await asyncio.sleep(float(step.get("seconds", 1)))
        elif action == "power_cycle":
            # Would use the node's power API
            log.info("Power cycle requested for %s", node)
        elif action == "set_boot_usb":
            log.info("Set boot USB requested for %s", node)

    # ── Query API ──────────────────────────────────────────────────────

    def get_result(self, test_id: str) -> dict | None:
        """Get result by ID (active or from history)."""
        if test_id in self._active:
            return self._active[test_id].to_dict()
        for r in self._history:
            if r.id == test_id:
                return r.to_dict()
        # Check disk
        result_file = RESULTS_DIR / test_id / "result.json"
        if result_file.exists():
            import json
            return json.loads(result_file.read_text())
        return None

    def list_results(self, limit: int = 50) -> list[dict]:
        """List recent test results."""
        results = [r.to_dict() for r in self._active.values()]
        results += [r.to_dict() for r in reversed(self._history)]
        # Also check disk
        for d in sorted(RESULTS_DIR.glob("test-*"), reverse=True)[:limit]:
            rf = d / "result.json"
            if rf.exists():
                import json
                results.append(json.loads(rf.read_text()))
        # Deduplicate by ID
        seen = set()
        unique = []
        for r in results:
            if r["id"] not in seen:
                seen.add(r["id"])
                unique.append(r)
        return unique[:limit]

    def abort(self, test_id: str) -> bool:
        """Abort a running test."""
        if test_id in self._abort_flags:
            self._abort_flags[test_id].set()
            return True
        return False
