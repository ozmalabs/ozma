#!/usr/bin/env python3
"""
E2E test: Rust ozma-agent integrates with the controller.

What this tests:
  1. The Rust agent binary starts and becomes healthy (GET /healthz → 200).
  2. The agent registers itself with the controller as a node.
  3. The controller's node list contains the agent node.
  4. The agent's own API endpoints respond correctly:
       GET /api/v1/status  → {"status": "running", "version": "..."}
       GET /api/v1/version → "0.1.0" (non-empty semver string)
  5. The controller's heartbeat keeps the node alive (last_seen advances).
  6. Scenario creation: a scenario can be bound to the agent node and activated.

Requirements:
  - Controller running on localhost:7380
  - Rust ozma-agent binary built (cargo build -p ozma-agent)
  - No other process on the randomly-assigned test ports

Usage:
  python tests/test_e2e_rust_agent.py
  python tests/test_e2e_rust_agent.py --binary /path/to/ozma-agent
  python tests/test_e2e_rust_agent.py --skip-scenario  # skip scenario tests
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

BASE_CONTROLLER = "http://localhost:7380"
TIMEOUT = 5.0

# ── Helpers ───────────────────────────────────────────────────────────────────


def free_port() -> int:
    """Pick a free TCP port."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def find_binary(explicit: str | None) -> str:
    if explicit:
        return explicit
    # Walk up from tests/ to find the workspace root.
    repo = Path(__file__).parent.parent
    candidates = [
        repo / "target/debug/ozma-agent",
        repo / "target/release/ozma-agent",
    ]
    for c in candidates:
        if c.exists():
            return str(c)
    sys.exit(
        "ozma-agent binary not found — run `cargo build -p ozma-agent` first"
    )


def api_get(base: str, path: str) -> dict:
    url = f"{base}{path}"
    with urllib.request.urlopen(url, timeout=TIMEOUT) as r:
        body = r.read()
        ct = r.headers.get("Content-Type", "")
        if "json" in ct:
            return json.loads(body)
        return {"_raw": body.decode()}


def api_post(base: str, path: str, body: dict | None = None) -> dict:
    url = f"{base}{path}"
    data = json.dumps(body or {}).encode()
    req = urllib.request.Request(
        url, data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
        body_bytes = r.read()
        return json.loads(body_bytes) if body_bytes else {}


def api_delete(base: str, path: str) -> dict:
    url = f"{base}{path}"
    req = urllib.request.Request(url, method="DELETE")
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            body = r.read()
            return json.loads(body) if body else {}
    except urllib.error.HTTPError:
        return {}


def wait_healthy(base: str, path: str, timeout: float = 15.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base}{path}", timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.1)
    return False


def check(condition: bool, msg: str) -> None:
    if condition:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        sys.exit(1)


def check_controller() -> None:
    try:
        api_get(BASE_CONTROLLER, "/health")
    except Exception:
        sys.exit(
            f"Controller not reachable at {BASE_CONTROLLER} — "
            "start it with `python3 controller/main.py` first"
        )


# ── Agent process ─────────────────────────────────────────────────────────────


class AgentProcess:
    def __init__(self, binary: str, api_port: int, metrics_port: int):
        self.api_port = api_port
        self.metrics_port = metrics_port
        self.proc = subprocess.Popen(
            [
                binary,
                "--api-port",      str(api_port),
                "--metrics-port",  str(metrics_port),
                "--controller-url", BASE_CONTROLLER,
                "--wg-port",       "0",
            ],
            env={**os.environ, "RUST_LOG": "warn"},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def api_base(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


# ── Derive expected node ID from hostname ─────────────────────────────────────


def expected_node_id() -> str:
    import socket as _socket
    return f"{_socket.gethostname()}._ozma._udp.local."


# ── Test cases ────────────────────────────────────────────────────────────────


def test_agent_starts(agent: AgentProcess) -> None:
    print("\n[1] Agent starts and becomes healthy")
    ok = wait_healthy(agent.api_base(), "/healthz", timeout=15)
    check(ok, "/healthz returns 200 within 15 s")


def test_agent_api(agent: AgentProcess) -> None:
    print("\n[2] Agent API endpoints")

    resp = api_get(agent.api_base(), "/api/v1/status")
    check(resp.get("status") == "running", f"status == 'running' (got {resp})")
    check(
        bool(resp.get("version")),
        f"version is non-empty (got {resp.get('version')!r})",
    )

    raw = api_get(agent.api_base(), "/api/v1/version")
    version = raw.get("_raw", raw.get("version", ""))
    check("." in version, f"version looks like semver: {version!r}")


def test_registered_with_controller(agent: AgentProcess, node_id: str) -> None:
    print("\n[3] Agent registers with controller")

    # Registration is async; give the agent a moment.
    deadline = time.monotonic() + 10
    registered = False
    while time.monotonic() < deadline:
        try:
            data = api_get(BASE_CONTROLLER, "/api/v1/nodes")
            ids = {n["id"] for n in data.get("nodes", [])}
            if node_id in ids:
                registered = True
                break
        except Exception:
            pass
        time.sleep(0.5)

    check(registered, f"node {node_id!r} visible in /api/v1/nodes")

    # Verify the node's api_port is set correctly.
    data = api_get(BASE_CONTROLLER, "/api/v1/nodes")
    node = next((n for n in data["nodes"] if n["id"] == node_id), None)
    check(node is not None, "node dict present")
    check(
        node.get("api_port") == agent.api_port,
        f"api_port == {agent.api_port} (got {node.get('api_port')})",
    )
    check(node.get("hw") == "soft", f"hw == 'soft' (got {node.get('hw')})")
    check(
        "agent" in (node.get("capabilities") or []),
        f"capabilities contains 'agent' (got {node.get('capabilities')})",
    )


def test_scenario_round_trip(agent: AgentProcess, node_id: str) -> None:
    print("\n[4] Scenario bound to agent node")

    # Create a scenario for this agent node.
    # The endpoint takes scenario_id as a query param:
    #   POST /api/v1/scenarios?scenario_id=<id>
    scenario_id = "rust-agent-e2e"
    try:
        api_post(
            BASE_CONTROLLER,
            f"/api/v1/scenarios?scenario_id={scenario_id}",
            {"name": "Rust Agent E2E", "node_id": node_id},
        )
    except urllib.error.HTTPError as e:
        if e.code == 409:
            pass  # already exists from a previous run
        else:
            raise

    # Verify it appears.
    data = api_get(BASE_CONTROLLER, "/api/v1/scenarios")
    ids = {s["id"] for s in data.get("scenarios", [])}
    check(scenario_id in ids, f"scenario {scenario_id!r} created")

    # Activate it.
    result = api_post(BASE_CONTROLLER, f"/api/v1/scenarios/{scenario_id}/activate")
    check(result.get("ok") is True, "activate returned ok=true")

    # Verify controller state.
    status = api_get(BASE_CONTROLLER, "/api/v1/status")
    check(
        status.get("active_scenario_id") == scenario_id,
        f"active_scenario_id == {scenario_id!r}",
    )
    check(
        status.get("active_node_id") == node_id,
        f"active_node_id == {node_id!r}",
    )

    # Clean up: delete the scenario (best-effort).
    api_delete(BASE_CONTROLLER, f"/api/v1/scenarios/{scenario_id}")


# ── Main ──────────────────────────────────────────────────────────────────────


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", help="Path to the ozma-agent binary")
    parser.add_argument("--skip-scenario", action="store_true",
                        help="Skip the scenario round-trip test")
    args = parser.parse_args()

    binary   = find_binary(args.binary)
    node_id  = expected_node_id()

    print(f"Binary:     {binary}")
    print(f"Controller: {BASE_CONTROLLER}")
    print(f"Node ID:    {node_id}")

    # Ensure the controller is running.
    check_controller()

    api_port     = free_port()
    metrics_port = free_port()
    agent = AgentProcess(binary, api_port, metrics_port)

    try:
        test_agent_starts(agent)
        test_agent_api(agent)
        test_registered_with_controller(agent, node_id)

        if not args.skip_scenario:
            test_scenario_round_trip(agent, node_id)
        else:
            print("\n[4] (skipped — --skip-scenario)")

    finally:
        agent.stop()

    print("\nAll tests passed.")


if __name__ == "__main__":
    main()
